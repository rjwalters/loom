//! Prompt-cache affinity preference for token selection (issue #8146).
//!
//! Anthropic's prompt cache is scoped **per account**, so a role tick only
//! reads its ~20–80k-token injected prefix (Claude Code system prompt + tool
//! schemas + repo `CLAUDE.md` + the expanded role prompt) from cache when it
//! happens to land on the same OAuth account as the previous tick of that same
//! `(repo, role)`. Measured over 378 role ticks on one host (#8146): ticks whose
//! previous same-role tick <60 min ago ran on the **same** account were a full
//! prefix hit 65% of the time; ticks that landed on a **different** account,
//! 1.4%. Rotation — the very thing the pool exists for — is what keeps the
//! caching machinery from paying off.
//!
//! This module records `(workspace, role) -> (account, timestamp)` in a small
//! state file alongside the pool's existing `.ranking` / `.bad_tokens` /
//! `.failure_counts`, and lets [`super::select`] **prefer** that account on the
//! next spawn for the same key.
//!
//! # What this is NOT
//!
//! A preference tier, **not** a new constraint. [`Affinity::pick`] only ever
//! returns an index into a candidate slice its caller already built, so an
//! affine account that is rate-limited, bad-marked, `.ranking`-hard-excluded,
//! non-Claude, or simply absent from the tier's own eligible set is passed over
//! exactly as it is today — there is no code path here that can admit an
//! account the existing algorithm refused. Every tier keeps its own exclusion
//! logic; affinity only reorders what survived it.
//!
//! # Bounds
//!
//! - **Off by default.** With no config and no env override, [`Affinity::resolve`]
//!   returns an inert value whose `pick` is always `None` and whose `record` is
//!   a no-op, so an unconfigured pool behaves bit-for-bit as it did before
//!   #8146 (and never grows a `.cache_affinity` file).
//! - **TTL.** A record older than [`AffinityConfig::ttl_secs`] (default
//!   [`DEFAULT_TTL_SECS`], ~1h — hits were observed up to 56 min out) yields no
//!   preference: past the cache window there is nothing left to hit anyway.
//! - **Quota guard.** The preference is dropped unless the affine account's own
//!   5h-window utilization (the `.ranking` third field, #4195) is *measured*
//!   and below [`AffinityConfig::max_util_5h`] (default
//!   [`DEFAULT_MAX_UTIL_5H`]), so affinity can never concentrate a repo's whole
//!   load on one account. The threshold sits **below** the tier-1 load gate
//!   (0.70) on purpose: cache savings are worth far less than quota headroom.
//!   An *unknown* utilization withdraws the preference rather than waiving the
//!   guard — see [`under_quota_guard`] for why this inverts the load gate's
//!   fail-open rule, and what it costs.
//! - **Role scoping.** `tokens.cacheAffinity.roles`, when non-empty, limits
//!   affinity to the named roles. This is how a fleet runs the support roles
//!   (`judge`, `guide`, …) with affinity while leaving sweeps — which spawn as
//!   `LOOM_ROLE=sweep-lifecycle`, deliberately fan out in parallel waves, and
//!   carry a different issue number in every prompt anyway — on plain rotation
//!   (the second phase #8146 describes).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::allowlist::atomic_write;
use super::locking::MkdirLock;

/// Basename of the affinity state file inside the resolved pool directory.
pub const STATE_FILE: &str = ".cache_affinity";

/// Enable/disable override. Truthy: `1`/`true`/`yes`/`on` (case-insensitive).
pub const ENABLED_ENV: &str = "LOOM_TOKEN_CACHE_AFFINITY";
/// TTL override, in seconds. A value `<= 0` or unparseable falls through.
pub const TTL_ENV: &str = "LOOM_TOKEN_CACHE_AFFINITY_TTL";
/// Quota-guard override (5h utilization fraction). A value `> 1.0` disables
/// the guard, mirroring `LOOM_TOKEN_5H_LOAD_GATE`.
pub const MAX_UTIL_ENV: &str = "LOOM_TOKEN_CACHE_AFFINITY_MAX_UTIL";
/// Role-scope override: comma-separated role names. Empty means "all roles".
pub const ROLES_ENV: &str = "LOOM_TOKEN_CACHE_AFFINITY_ROLES";

/// Default cache window. The observed prompt-cache TTL is ~1h (#8146 recorded
/// a hit 56 min after the warming tick).
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// Default quota guard: affinity applies only while the affine account is
/// below half of its 5h window. Deliberately stricter than the tier-1 load
/// gate (`DEFAULT_5H_LOAD_GATE` = 0.70) — the pool's reason to exist is that
/// one weekly limit cannot stall the pipeline, and a cache hit is never worth
/// trading that away.
pub const DEFAULT_MAX_UTIL_5H: f64 = 0.50;

/// Records older than this are dropped the next time the file is written, so
/// a long-lived pool's state file stays bounded by its live `(repo, role)`
/// set rather than by every combination ever spawned.
const MAX_RETAINED_AGE_SECS: i64 = 7 * 24 * 3600;

/// Resolved affinity settings for one workspace. See the module docs for what
/// each bound is for.
#[derive(Debug, Clone, PartialEq)]
pub struct AffinityConfig {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub max_util_5h: f64,
    /// Roles affinity applies to. Empty = every role.
    pub roles: Vec<String>,
}

impl Default for AffinityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: DEFAULT_TTL_SECS,
            max_util_5h: DEFAULT_MAX_UTIL_5H,
            roles: Vec::new(),
        }
    }
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn split_roles(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve `tokens.cacheAffinity.*` for `workspace`: **env > config >
/// default**, the precedence every other Loom knob uses. Every read soft-fails
/// — a missing file, a malformed value, or a wrong JSON type all leave the
/// default in place rather than failing a spawn over bookkeeping config.
#[must_use]
pub fn resolve_config(workspace: &Path) -> AffinityConfig {
    let effective = crate::config_resolver::resolve_effective_config(workspace);
    let get = |key: &str| crate::config_resolver::get_path(&effective, key).cloned();
    let mut cfg = AffinityConfig::default();

    if let Some(v) = get("tokens.cacheAffinity.enabled").and_then(|v| v.as_bool()) {
        cfg.enabled = v;
    }
    if let Ok(raw) = std::env::var(ENABLED_ENV) {
        if let Some(v) = parse_bool(&raw) {
            cfg.enabled = v;
        }
    }

    if let Some(v) = get("tokens.cacheAffinity.ttlSeconds").and_then(|v| v.as_i64()) {
        if v > 0 {
            cfg.ttl_secs = v as u64;
        }
    }
    if let Ok(raw) = std::env::var(TTL_ENV) {
        if let Ok(v) = raw.trim().parse::<i64>() {
            if v > 0 {
                cfg.ttl_secs = v as u64;
            }
        }
    }

    if let Some(v) = get("tokens.cacheAffinity.maxUtil5h").and_then(|v| v.as_f64()) {
        cfg.max_util_5h = v;
    }
    if let Ok(raw) = std::env::var(MAX_UTIL_ENV) {
        if let Ok(v) = raw.trim().parse::<f64>() {
            cfg.max_util_5h = v;
        }
    }

    if let Some(serde_json::Value::Array(items)) = get("tokens.cacheAffinity.roles") {
        cfg.roles = items
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(raw) = std::env::var(ROLES_ENV) {
        cfg.roles = split_roles(&raw);
    }

    cfg
}

/// One `(workspace, role) -> (account, timestamp)` record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AffinityEntry {
    account: String,
    /// ISO-8601 UTC, `%Y-%m-%dT%H:%M:%SZ` — the same stamp format
    /// `.failure_counts` and `.bad_tokens` use.
    at: String,
}

type State = BTreeMap<String, AffinityEntry>;

fn state_path(tokens_dir: &Path) -> PathBuf {
    tokens_dir.join(STATE_FILE)
}

fn lock_path(tokens_dir: &Path) -> PathBuf {
    tokens_dir.join(format!("{STATE_FILE}.lock"))
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_stamp(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Tolerant read: a missing, empty, or malformed file is an empty map, and an
/// individual unparseable row is dropped rather than poisoning the rest —
/// matching [`super::failure_counts`]'s contract. Losing an affinity record
/// costs one cache write; refusing to select costs a spawn.
fn read_state(path: &Path) -> State {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return State::new();
    };
    if raw.trim().is_empty() {
        return State::new();
    }
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return State::new();
    };
    let mut cleaned = State::new();
    for (key, value) in map {
        let serde_json::Value::Object(obj) = value else {
            continue;
        };
        let (Some(account), Some(at)) = (
            obj.get("account").and_then(|v| v.as_str()),
            obj.get("at").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if account.trim().is_empty() || parse_stamp(at).is_none() {
            continue;
        }
        cleaned.insert(
            key,
            AffinityEntry {
                account: account.to_string(),
                at: at.to_string(),
            },
        );
    }
    cleaned
}

/// The `(workspace, role)` key. The workspace is part of the key because a
/// shared machine-level pool (#3938) serves many repos, and each repo's
/// `CLAUDE.md` is a different prefix.
#[must_use]
pub fn affinity_key(workspace: &Path, role: &str) -> String {
    format!("{}|{}", workspace.display(), role.trim())
}

/// A resolved affinity decision for one selection attempt.
///
/// Construct with [`Affinity::resolve`] once per `select_token` call; the two
/// methods are then cheap and side-effect-free until [`Affinity::record`].
#[derive(Debug, Clone, Default)]
pub struct Affinity {
    /// `None` when affinity is off, the role is unknown, or the role is out of
    /// scope — in that state nothing is read, preferred, or written.
    key: Option<String>,
    /// The account to prefer, when one is recorded, still inside the TTL, and
    /// under the quota guard.
    preferred: Option<String>,
}

impl Affinity {
    /// Inert affinity: no preference, no recording. The value every caller
    /// that has no role identity (in-process selection, `tokens select`
    /// without `--role`) gets.
    #[must_use]
    pub fn inert() -> Self {
        Self::default()
    }

    /// Resolve the preference for `(workspace, role)` against the pool at
    /// `tokens_dir`.
    ///
    /// Returns [`Affinity::inert`] — reading nothing at all — when `role` is
    /// `None`, when the feature is disabled (the default), or when the role is
    /// outside a configured `roles` scope. Otherwise the recorded account is
    /// admitted only if the record is within the TTL and the account's 5h
    /// utilization is under the quota guard.
    ///
    /// Note what is deliberately *not* checked here: whether the account is
    /// bad-marked, hard-excluded, or otherwise spawnable. That is the tiers'
    /// job, and [`Affinity::pick`] can only ever return a candidate the tier
    /// itself produced — so re-deriving eligibility here would duplicate
    /// exclusion logic without being able to weaken it.
    #[must_use]
    pub fn resolve(workspace: &Path, tokens_dir: &Path, role: Option<&str>) -> Self {
        let Some(role) = role.map(str::trim).filter(|r| !r.is_empty()) else {
            return Self::inert();
        };
        // One `resolve_effective_config` read per selection, on the spawn hot
        // path, even when affinity is disabled. Deliberate and not cached: a
        // cache would have to be invalidated on config edits the daemon does
        // not observe, and the cost is small next to the `.ranking`,
        // `.bad_tokens`, `.allowlist` and `index.json` reads the same
        // selection already performs. The `role`-is-`None` fast path above
        // means a caller that never sets `LOOM_ROLE` does not pay it at all.
        let config = resolve_config(workspace);
        if !config.enabled {
            return Self::inert();
        }
        if !config.roles.is_empty() && !config.roles.iter().any(|r| r == role) {
            return Self::inert();
        }
        let key = affinity_key(workspace, role);
        let preferred = read_state(&state_path(tokens_dir))
            .get(&key)
            .and_then(|entry| {
                let at = parse_stamp(&entry.at)?;
                let age = chrono::Utc::now().signed_duration_since(at).num_seconds();
                // A future-dated stamp (clock skew between fleet hosts sharing
                // a pool) is treated as "just now", never as an expired record.
                if age > config.ttl_secs as i64 {
                    return None;
                }
                Some(entry.account.clone())
            })
            .filter(|account| under_quota_guard(tokens_dir, account, config.max_util_5h));
        Self {
            key: Some(key),
            preferred,
        }
    }

    /// The account this selection prefers, if any.
    #[must_use]
    pub fn preferred(&self) -> Option<&str> {
        self.preferred.as_deref()
    }

    /// Index of the preferred account within `items`, or `None`.
    ///
    /// This is the **only** way affinity influences a pick, and it is why
    /// affinity cannot select an account the current algorithm would refuse:
    /// the returned index always addresses a candidate the caller already
    /// admitted.
    #[must_use]
    pub fn pick<T>(&self, items: &[T], name_of: impl Fn(&T) -> String) -> Option<usize> {
        let preferred = self.preferred.as_deref()?;
        items.iter().position(|item| name_of(item) == preferred)
    }

    /// Record `account` as the account that just warmed this key's cache.
    ///
    /// Best-effort and no-op when inert: a lock timeout or I/O failure costs
    /// one future cache hit, and must never fail a spawn that already has a
    /// token in hand. The timestamp is refreshed on every selection (including
    /// one that reused the affine account), because a cache read also extends
    /// the upstream entry's own TTL.
    pub fn record(&self, tokens_dir: &Path, account: &str) {
        let Some(key) = self.key.as_deref() else {
            return;
        };
        if std::fs::create_dir_all(tokens_dir).is_err() {
            return;
        }
        let Ok(_lock) = MkdirLock::acquire(&lock_path(tokens_dir)) else {
            return;
        };
        let path = state_path(tokens_dir);
        let mut state = read_state(&path);
        let now = chrono::Utc::now();
        state.retain(|_, entry| {
            parse_stamp(&entry.at).is_some_and(|at| {
                now.signed_duration_since(at).num_seconds() < MAX_RETAINED_AGE_SECS
            })
        });
        state.insert(
            key.to_string(),
            AffinityEntry {
                account: account.to_string(),
                at: now_iso(),
            },
        );
        if let Ok(mut body) = serde_json::to_string_pretty(&state) {
            body.push('\n');
            let _ = atomic_write(&path, &body);
        }
    }
}

/// Whether `account` has a **measured** 5h-window utilization below
/// `max_util`.
///
/// The guard is evidence-requiring, not evidence-waiving: an **unknown**
/// utilization — no `.ranking` row for the account, a legacy 2-field row, an
/// unparseable field, or no `.ranking` file at all — withdraws the preference.
/// So the invariant this upholds is unconditional:
///
/// > affinity is applied only to an account whose measured 5h utilization is
/// > strictly below the configured threshold.
///
/// This is deliberately the **opposite** of the tier-1 load gate's "unknown →
/// never gated" (#4195), and the asymmetry is the point. The load gate is an
/// *exclusion*: gating on unknown there would shrink the candidate set and
/// could empty a live pool, so it fails open. This guard only decides whether
/// to express a *preference* among candidates some tier already admitted, so
/// failing closed costs exactly one cache hit and falls back to the rotation
/// that shipped before #8146 — it can never empty the pool or hand out a
/// different account than the existing algorithm would have.
///
/// It matters because `record` refreshes the stamp on every reuse, so a key
/// whose role ticks more often than once per TTL stays pinned indefinitely;
/// the quota guard is then the *only* live bound on concentration. Waiving it
/// whenever telemetry is missing — precisely when a pool is least observable —
/// would make the bound vacuous exactly when it is needed. The cost is that a
/// pool with no ranking refresher gets no affinity; that is documented in
/// `token-pool.md`, and such a pool has no load gating today either.
///
/// Unlike tier 1, the `.ranking` is read at any age: a stale-but-low
/// measurement is still evidence the account is not saturated, and a
/// stale-but-high one still withdraws. A threshold `> 1.0` disables the guard
/// entirely, mirroring `LOOM_TOKEN_5H_LOAD_GATE`, and is the documented escape
/// hatch for an operator who wants affinity on a pool without ranking data.
pub(super) fn under_quota_guard(tokens_dir: &Path, account: &str, max_util: f64) -> bool {
    if max_util > 1.0 {
        return true;
    }
    match super::select::ranking_util_5h(tokens_dir, account) {
        Some(util) => util < max_util,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    fn workspace_with_config(config: Option<&str>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".loom").join("tokens")).unwrap();
        if let Some(body) = config {
            fs::write(tmp.path().join(".loom").join("config.json"), body).unwrap();
        }
        tmp
    }

    fn pool(ws: &Path) -> PathBuf {
        ws.join(".loom").join("tokens")
    }

    const ENABLED_CONFIG: &str = r#"{"tokens": {"cacheAffinity": {"enabled": true}}}"#;

    /// Give `account` a measured 5h utilization, which the quota guard
    /// **requires** before it will admit any preference at all. Most tests
    /// below are about some other bound (TTL, role scope, config precedence),
    /// so they call this once with a comfortably-under-threshold figure to get
    /// the guard out of the way.
    fn rank(tokens_dir: &Path, account: &str, util: f64) {
        fs::write(tokens_dir.join(".ranking"), format!("{account}|available|{util}\n")).unwrap();
    }

    /// Write an affinity record `age_secs` old, bypassing `record` so the test
    /// controls the timestamp.
    fn seed(tokens_dir: &Path, key: &str, account: &str, age_secs: i64) {
        let at = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let mut state = read_state(&state_path(tokens_dir));
        state.insert(
            key.to_string(),
            AffinityEntry {
                account: account.to_string(),
                at,
            },
        );
        let mut body = serde_json::to_string_pretty(&state).unwrap();
        body.push('\n');
        atomic_write(&state_path(tokens_dir), &body).unwrap();
    }

    #[test]
    #[serial]
    fn unconfigured_is_inert_and_writes_nothing() {
        let tmp = workspace_with_config(None);
        let affinity = Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"));
        assert!(affinity.preferred().is_none());
        affinity.record(&pool(tmp.path()), "agent1");
        assert!(
            !state_path(&pool(tmp.path())).exists(),
            "an unconfigured pool must never grow a {STATE_FILE} file"
        );
    }

    #[test]
    #[serial]
    fn no_role_is_inert_even_when_enabled() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        let affinity = Affinity::resolve(tmp.path(), &pool(tmp.path()), None);
        assert!(affinity.preferred().is_none());
        affinity.record(&pool(tmp.path()), "agent1");
        assert!(!state_path(&pool(tmp.path())).exists());
    }

    #[test]
    #[serial]
    fn records_then_prefers_same_account_for_same_key() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        rank(&pool(tmp.path()), "agent3", 0.10);
        Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"))
            .record(&pool(tmp.path()), "agent3");
        let next = Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"));
        assert_eq!(next.preferred(), Some("agent3"));
        // A different role is a different cache prefix — no preference.
        let other = Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("guide"));
        assert_eq!(other.preferred(), None);
    }

    #[test]
    #[serial]
    fn record_past_ttl_yields_no_preference() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        rank(&pool(tmp.path()), "agent3", 0.10);
        let key = affinity_key(tmp.path(), "judge");
        seed(&pool(tmp.path()), &key, "agent3", DEFAULT_TTL_SECS as i64 + 60);
        let affinity = Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"));
        assert_eq!(affinity.preferred(), None);
        // Inside the window the same record IS preferred — proving the only
        // difference above was age.
        seed(&pool(tmp.path()), &key, "agent3", 60);
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
    }

    #[test]
    #[serial]
    fn quota_guard_drops_preference_at_threshold() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        let key = affinity_key(tmp.path(), "judge");
        seed(&pool(tmp.path()), &key, "agent3", 60);
        // Under the default 0.50 guard.
        fs::write(pool(tmp.path()).join(".ranking"), "agent3|available|0.40\n").unwrap();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
        // At/above it, affinity steps aside and ordinary rotation applies.
        fs::write(pool(tmp.path()).join(".ranking"), "agent3|available|0.50\n").unwrap();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
    }

    /// The guard requires evidence rather than waiving itself without it —
    /// inverting the tier-1 load gate's "unknown → never gated" (#4195) on
    /// purpose, so that "affinity only ever prefers an account measured below
    /// the threshold" holds unconditionally instead of only on a pool whose
    /// ranking happens to be populated. See `under_quota_guard`.
    #[test]
    #[serial]
    fn unknown_utilisation_withdraws_the_preference() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        let key = affinity_key(tmp.path(), "judge");
        seed(&pool(tmp.path()), &key, "agent3", 60);

        // No `.ranking` at all.
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
        // A legacy 2-field row carries no utilization.
        fs::write(pool(tmp.path()).join(".ranking"), "agent3|available\n").unwrap();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
        // A ranking that measures some *other* account says nothing about this
        // one — an unranked affine account is unknown, not zero.
        fs::write(pool(tmp.path()).join(".ranking"), "agent9|available|0.01\n").unwrap();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
        // Control: the very same record IS preferred the moment a utilization
        // exists, proving the three cases above turn on evidence and nothing
        // else.
        rank(&pool(tmp.path()), "agent3", 0.10);
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
    }

    /// The documented escape hatch for a pool with no ranking data: a threshold
    /// `> 1.0` disables the guard outright, exactly like `LOOM_TOKEN_5H_LOAD_GATE`.
    #[test]
    #[serial]
    fn a_threshold_above_one_disables_the_guard_entirely() {
        let tmp = workspace_with_config(Some(
            r#"{"tokens": {"cacheAffinity": {"enabled": true, "maxUtil5h": 1.5}}}"#,
        ));
        seed(&pool(tmp.path()), &affinity_key(tmp.path(), "judge"), "agent3", 60);
        // No `.ranking` whatsoever, yet the preference stands.
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
        // …and a fully-saturated account is likewise not withdrawn, which is
        // what "disabled" has to mean for the hatch to be one.
        fs::write(pool(tmp.path()).join(".ranking"), "agent3|available|0.99\n").unwrap();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
    }

    /// A *stale* `.ranking` is still read (tier 1 would have skipped it): a
    /// stale-but-low measurement is evidence the account is not saturated, and
    /// a stale-but-high one still withdraws.
    #[test]
    #[serial]
    fn stale_ranking_still_feeds_the_guard_in_both_directions() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        seed(&pool(tmp.path()), &affinity_key(tmp.path(), "judge"), "agent3", 60);
        let ranking = pool(tmp.path()).join(".ranking");
        let backdate = || {
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(700);
            fs::File::open(&ranking).unwrap().set_modified(old).unwrap();
        };

        fs::write(&ranking, "agent3|available|0.10\n").unwrap();
        backdate();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );

        fs::write(&ranking, "agent3|available|0.80\n").unwrap();
        backdate();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
    }

    #[test]
    #[serial]
    fn configured_roles_scope_the_preference() {
        let tmp = workspace_with_config(Some(
            r#"{"tokens": {"cacheAffinity": {"enabled": true, "roles": ["judge", "guide"]}}}"#,
        ));
        rank(&pool(tmp.path()), "agent3", 0.10);
        for role in ["judge", "guide", "sweep-lifecycle"] {
            seed(&pool(tmp.path()), &affinity_key(tmp.path(), role), "agent3", 60);
        }
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("sweep-lifecycle")).preferred(),
            None,
            "a role outside the configured scope must get no preference"
        );
        // …and an out-of-scope role must not record either.
        let before = fs::read_to_string(state_path(&pool(tmp.path()))).unwrap();
        Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("sweep-lifecycle"))
            .record(&pool(tmp.path()), "agent9");
        assert_eq!(fs::read_to_string(state_path(&pool(tmp.path()))).unwrap(), before);
    }

    #[test]
    #[serial]
    fn env_overrides_config() {
        let tmp =
            workspace_with_config(Some(r#"{"tokens": {"cacheAffinity": {"enabled": false}}}"#));
        rank(&pool(tmp.path()), "agent3", 0.10);
        seed(&pool(tmp.path()), &affinity_key(tmp.path(), "judge"), "agent3", 60);
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
        std::env::set_var(ENABLED_ENV, "1");
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
        // A 1s TTL expires the same 60s-old record.
        std::env::set_var(TTL_ENV, "1");
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
        std::env::remove_var(TTL_ENV);
        std::env::remove_var(ENABLED_ENV);
    }

    #[test]
    #[serial]
    fn pick_only_ever_indexes_the_candidates_it_was_given() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        rank(&pool(tmp.path()), "agent3", 0.10);
        seed(&pool(tmp.path()), &affinity_key(tmp.path(), "judge"), "agent3", 60);
        let affinity = Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"));
        let candidates = ["agent1".to_string(), "agent3".to_string()];
        assert_eq!(affinity.pick(&candidates, Clone::clone), Some(1));
        // The affine account absent from the candidate set (excluded upstream
        // by whatever rule) yields no pick at all — never a fabricated index.
        let without = ["agent1".to_string(), "agent2".to_string()];
        assert_eq!(affinity.pick(&without, Clone::clone), None);
        assert_eq!(Affinity::inert().pick(&candidates, Clone::clone), None);
    }

    #[test]
    #[serial]
    fn malformed_state_file_is_treated_as_empty() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        rank(&pool(tmp.path()), "agent3", 0.10);
        fs::write(state_path(&pool(tmp.path())), "{not json").unwrap();
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            None
        );
        // …and a later record rewrites it cleanly rather than failing.
        Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"))
            .record(&pool(tmp.path()), "agent3");
        assert_eq!(
            Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge")).preferred(),
            Some("agent3")
        );
    }

    #[test]
    #[serial]
    fn ancient_records_are_pruned_on_write() {
        let tmp = workspace_with_config(Some(ENABLED_CONFIG));
        seed(
            &pool(tmp.path()),
            &affinity_key(tmp.path(), "retired-role"),
            "agent1",
            MAX_RETAINED_AGE_SECS + 60,
        );
        Affinity::resolve(tmp.path(), &pool(tmp.path()), Some("judge"))
            .record(&pool(tmp.path()), "agent3");
        let state = read_state(&state_path(&pool(tmp.path())));
        assert!(state.contains_key(&affinity_key(tmp.path(), "judge")));
        assert!(
            !state.contains_key(&affinity_key(tmp.path(), "retired-role")),
            "records past the retention window must be dropped, not accumulated"
        );
    }
}
