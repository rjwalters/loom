//! A reads-only pool of GitHub Apps, so one host can spend more than one
//! installation's REST budget (#9248).
//!
//! # Why
//!
//! A GitHub App *installation* has a core REST ceiling of
//! `max(5000, 4000 + 50 * repos)` — about 6,900/hour for a 58-repo org. The
//! fleet's steady forge polling measured ~7,400/hour against it: a structural
//! overdraw, not a burst. On 2026-09-27 that exhausted the installation and
//! paused forge polling on two hosts for 16 minutes.
//!
//! Several permission-identical Apps installed over the same repos each carry
//! their **own** independent ceiling and window. The pre-#9248 way to use them
//! was per-host pinning (`forge.githubApp` in each host's machine-tier config),
//! which has three defects this module exists to remove:
//!
//! - the budget is lopsided — whichever App carries the busiest hosts runs out
//!   first while the others sit idle;
//! - one App can take a whole host down;
//! - adding an App means *moving a host*, rather than simply adding headroom.
//!
//! # Reads only, deliberately
//!
//! Each installation token acts as its own `<slug>[bot]` account. Pooling
//! **writes** would make the author of comments, labels and merges
//! unpredictable, which in turn affects ruleset bypass actors, bot-login
//! matching (claims, leases, "already commented" checks, Judge/Champion author
//! checks), auto-merge arming, and the audit trail. The measured drain is
//! almost entirely reads. So this module is consulted **only** on read paths,
//! and [`select_for_repo`] is deliberately not reachable from any write call
//! site — one write identity survives.
//!
//! # Absent ⇒ nothing changes
//!
//! [`configured_pool`] returns an empty vec when no pool is configured, and
//! every entry point here treats an empty pool as "no pool feature exists on
//! this host". Callers then fall through to the existing single-App path
//! byte-for-byte.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::forge_bucket_book::Resource;

/// Env override for the read pool, highest precedence — mirroring
/// `LOOM_GITHUB_APP_ID` / `LOOM_GITHUB_APP_KEY_PATH`'s precedence over
/// `forge.githubApp.*` in `github-app-token.sh`.
///
/// Format: comma-separated `<appId>:<privateKeyPath>` pairs, e.g.
/// `5100879:/home/u/.loom/github-app/a.pem,5101048:/home/u/.loom/github-app/b.pem`.
/// A malformed entry is skipped rather than failing the host closed — a broken
/// override must not be able to take forge reads down, since the single-App
/// path remains available.
pub const READ_POOL_ENV: &str = "LOOM_GITHUB_APP_READ_POOL";

/// How long a member stays withdrawn after an exhaustion/auth failure when the
/// forge gave us no explicit reset instant to use instead.
///
/// Deliberately short. A withdrawal is a *hint*, not a ledger: the cost of
/// re-probing a still-exhausted member is one wasted request, while the cost of
/// over-withdrawing is sending everything to the remaining members and
/// exhausting them too.
pub const DEFAULT_WITHDRAWAL: Duration = Duration::from_secs(300);

/// One App the fleet may mint **read** installation tokens from.
///
/// Every member must be permission-identical to the primary: a member with a
/// narrower permission set 403s where another succeeds, which presents as a
/// routing-dependent flake rather than a clean failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolMember {
    /// The GitHub App id, as a string — it is only ever passed through to
    /// `github-app-token.sh` and used as a path segment, never arithmetic.
    pub app_id: String,
    /// Absolute path to the App's private key (`0600`, host-local, never
    /// committed).
    pub private_key_path: PathBuf,
}

/// Resolve the configured read pool: [`READ_POOL_ENV`] first, else
/// `forge.githubAppReadPool` from the effective config.
///
/// An empty result means "no pool on this host" and is the load-bearing
/// default — see the module docs.
#[must_use]
pub fn configured_pool(repo_root: &Path) -> Vec<PoolMember> {
    if let Some(from_env) = env_pool() {
        return from_env;
    }
    config_pool(repo_root)
}

/// [`READ_POOL_ENV`], parsed. `None` when unset/blank so the caller falls
/// through to config; `Some(vec![])` is never returned — an override that
/// parses to nothing is indistinguishable from "not set" on purpose.
fn env_pool() -> Option<Vec<PoolMember>> {
    let raw = std::env::var(READ_POOL_ENV).ok()?;
    let members: Vec<PoolMember> = raw
        .split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            // Split on the FIRST colon: an app id is digits and never contains
            // one, whereas a path may (`C:\keys\a.pem`), so splitting from the
            // right would cut the path instead of the separator.
            let (app_id, key) = entry.split_once(':')?;
            member(app_id, key)
        })
        .collect();
    (!members.is_empty()).then_some(members)
}

/// `forge.githubAppReadPool` — an array of `{appId, privateKeyPath}` objects.
fn config_pool(repo_root: &Path) -> Vec<PoolMember> {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(list) = crate::config_resolver::get_path(&effective, "forge.githubAppReadPool") else {
        return Vec::new();
    };
    let Some(entries) = list.as_array() else {
        return Vec::new();
    };
    entries.iter().filter_map(member_from_value).collect()
}

/// One `{appId, privateKeyPath}` object -> [`PoolMember`]. `appId` is accepted
/// as either a JSON string or number, because both spellings occur in
/// hand-written config and neither is wrong.
fn member_from_value(value: &Value) -> Option<PoolMember> {
    let app_id = value.get("appId").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })?;
    let key = value.get("privateKeyPath").and_then(Value::as_str)?;
    member(&app_id, key)
}

/// Build a member, rejecting blank parts. Keeps the two parse paths honest
/// about the same emptiness rule.
fn member(app_id: &str, key_path: &str) -> Option<PoolMember> {
    let app_id = app_id.trim();
    let key_path = key_path.trim();
    (!app_id.is_empty() && !key_path.is_empty()).then(|| PoolMember {
        app_id: app_id.to_string(),
        private_key_path: PathBuf::from(key_path),
    })
}

// ---------------------------------------------------------------------------
// Deterministic assignment
// ---------------------------------------------------------------------------

/// Stable index for `owner_repo` across a pool of `len` members.
///
/// **SHA-256, not [`std::collections::hash_map::DefaultHasher`].** The
/// assignment must be identical on every host and across Rust releases so two
/// daemons polling the same repo agree without coordinating; `DefaultHasher`
/// guarantees neither (it is explicitly documented as unstable across
/// releases, and is randomly seeded per process for `HashMap`). The same
/// reasoning already governs `tokens_pool::select::pool_account_fingerprint`.
#[must_use]
pub fn assignment_index(owner_repo: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"loom-read-pool/v1:");
    hasher.update(owner_repo.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    Some((u64::from_be_bytes(bytes) % len as u64) as usize)
}

/// SHA-256 of `domain` + `owner_repo` (lowercased) + `|` + `affinity_key`,
/// first 8 bytes big-endian, mod `len` — the shared shape of
/// [`split_index`] and [`spill_index`].
fn keyed_index(domain: &[u8], owner_repo: &str, affinity_key: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(domain);
    hasher.update(owner_repo.to_ascii_lowercase().as_bytes());
    hasher.update(b"|");
    hasher.update(affinity_key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    Some((u64::from_be_bytes(bytes) % len as u64) as usize)
}

/// The reader index that serves one request of a **split** repo (W4-B):
/// SHA-256 of `"loom-read-pool/split/v1:" + owner_repo (lowercased) + "|" +
/// affinity_key`, first 8 bytes big-endian, mod `len`.
///
/// Per request rather than per repo, so a hot repo's reads spread across
/// the pool, but keyed by the request's identity
/// ([`crate::gh_invocation::affinity_key`]), so one URL always lands on one
/// reader and keeps that reader's ETag. Same cross-host contract as
/// [`assignment_index`]: changing it moves every split URL once.
#[must_use]
pub fn split_index(owner_repo: &str, affinity_key: &str, len: usize) -> Option<usize> {
    keyed_index(b"loom-read-pool/split/v1:", owner_repo, affinity_key, len)
}

/// The spill hash of one request (W4-B): SHA-256 of
/// `"loom-read-pool/spill/v1:" + owner_repo (lowercased) + "|" +
/// affinity_key`, first 8 bytes big-endian, mod `len`. A **partial** spill
/// moves exactly the requests with `spill_index(.., 2) == Some(1)`, so the
/// moved half is the same on every host and every tick of the episode.
#[must_use]
pub fn spill_index(owner_repo: &str, affinity_key: &str, len: usize) -> Option<usize> {
    keyed_index(b"loom-read-pool/spill/v1:", owner_repo, affinity_key, len)
}

/// Pick the member that should serve reads for `owner_repo`, skipping any
/// member currently withdrawn.
///
/// Walks forward from the hashed index so the fallback order is itself
/// deterministic: two hosts that both see the same member withdrawn land on the
/// same replacement. Returns `None` when the pool is empty **or every member is
/// withdrawn** — the caller then falls back to the primary/pinned App, and it is
/// that fallback, not this function, that lets the rate-limit breaker trip only
/// once nothing is left.
#[must_use]
pub fn select_for_repo<'a>(pool: &'a [PoolMember], owner_repo: &str) -> Option<&'a PoolMember> {
    select_for_repo_at(pool, owner_repo, SystemTime::now())
}

/// [`select_for_repo`] with an injected clock, so the withdrawal behaviour is
/// testable without sleeping.
#[must_use]
pub fn select_for_repo_at<'a>(
    pool: &'a [PoolMember],
    owner_repo: &str,
    now: SystemTime,
) -> Option<&'a PoolMember> {
    // #9986: the gateway owns the pool on a `required` egress host.
    // `workspace_root()` is `None` when `WORKSPACE_ROOT` was never registered
    // (CLI subcommands, not the daemon). Then only the env/machine policy tiers
    // are consulted, so a repo-tier-only `required` policy is not honoured
    // here — a deliberate fail-open for the repo tier alone: the daemon (which
    // mints and publishes) registers its workspace at startup via
    // `forge_identity::spawn_reader_refresh`, and
    // env/machine `required` policies still apply.
    if crate::forge_egress::publication::github_credential_forbidden(
        crate::forge_identity::workspace_root(),
    ) {
        return None;
    }
    let start = assignment_index(owner_repo, pool.len())?;
    walk_order(start, pool.len())
        .map(|i| &pool[i])
        .find(|member| !is_withdrawn_at(&member.app_id, now))
}

/// The one deterministic walk every reader choice uses (W4-B): `start`,
/// then forward around a pool of `len`. Two hosts that agree on `start` and
/// on which members are unusable land on the same member.
pub fn walk_order(start: usize, len: usize) -> impl Iterator<Item = usize> {
    (0..len).map(move |offset| (start + offset) % len)
}

// ---------------------------------------------------------------------------
// Withdrawal of exhausted or broken members
// ---------------------------------------------------------------------------

/// `app_id` -> the instant it becomes eligible again.
fn withdrawals() -> &'static Mutex<HashMap<String, SystemTime>> {
    static WITHDRAWN: OnceLock<Mutex<HashMap<String, SystemTime>>> = OnceLock::new();
    WITHDRAWN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Withdraw `app_id` until `until`.
///
/// Call this when a member's installation is rate-limited, or returns an auth
/// or coverage error for a repo: the member is skipped for subsequent reads
/// rather than tripping the whole host's breaker. An existing withdrawal is
/// only ever *extended*, never shortened, so a later optimistic reading cannot
/// readmit a member another call site just found exhausted.
pub fn withdraw_until(app_id: &str, until: SystemTime) {
    let Ok(mut map) = withdrawals().lock() else {
        return; // a poisoned lock must not take reads down
    };
    map.entry(app_id.to_string())
        .and_modify(|existing| {
            if until > *existing {
                *existing = until;
            }
        })
        .or_insert(until);
}

/// Withdraw `app_id` for [`DEFAULT_WITHDRAWAL`] from now — for the common case
/// where the forge told us a member is unusable but gave no reset instant.
pub fn withdraw(app_id: &str) {
    withdraw_until(app_id, SystemTime::now() + DEFAULT_WITHDRAWAL);
}

/// Whether `app_id` is currently withdrawn.
#[must_use]
pub fn is_withdrawn(app_id: &str) -> bool {
    is_withdrawn_at(app_id, SystemTime::now())
}

/// [`is_withdrawn`] against an explicit clock.
#[must_use]
pub fn is_withdrawn_at(app_id: &str, now: SystemTime) -> bool {
    let Ok(map) = withdrawals().lock() else {
        return false; // fail open: a poisoned lock must not withdraw everything
    };
    map.get(app_id).is_some_and(|&until| now < until)
}

// ---------------------------------------------------------------------------
// Withdrawal scoped to one (app, owner, resource) bucket (W4-A)
// ---------------------------------------------------------------------------

/// Which of an installation's rate-limit pools a scoped withdrawal covers.
///
/// GitHub meters each `(App, owner)` installation separately and, within it,
/// each resource separately: an exhausted `core` pool for one owner says
/// nothing about the same App's `graphql` pool, or about its installation on
/// another owner. [`ResourceScope::All`] is for failures that are not about
/// one pool — a secondary (abuse/concurrency) limit, or bad credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceScope {
    Core,
    Graphql,
    Search,
    /// Every resource of the installation.
    All,
}

impl ResourceScope {
    /// The scope covering exactly `resource`.
    #[must_use]
    pub fn of(resource: Resource) -> Self {
        match resource {
            Resource::Core => Self::Core,
            Resource::Graphql => Self::Graphql,
            Resource::Search => Self::Search,
        }
    }

    /// Whether this scope covers `resource`.
    #[must_use]
    pub fn covers(self, resource: Resource) -> bool {
        self == Self::All || self == Self::of(resource)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Graphql => "graphql",
            Self::Search => "search",
            Self::All => "all",
        }
    }
}

/// `(app id, owner lowercased, scope)` -> the instant it becomes eligible.
type ScopedKey = (String, String, ResourceScope);

fn scoped_withdrawals() -> &'static Mutex<HashMap<ScopedKey, SystemTime>> {
    static SCOPED: OnceLock<Mutex<HashMap<ScopedKey, SystemTime>>> = OnceLock::new();
    SCOPED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Withdraw reader `app_id` from `owner`'s `scope` until `until`. Like
/// [`withdraw_until`], an existing withdrawal is only ever extended, never
/// shortened. Returns the instant the withdrawal now ends (the longer of the
/// held and the requested one).
pub fn withdraw_scoped_until(
    app_id: &str,
    owner: &str,
    scope: ResourceScope,
    until: SystemTime,
) -> SystemTime {
    let Ok(mut map) = scoped_withdrawals().lock() else {
        return until; // a poisoned lock must not take reads down
    };
    let held = map
        .entry((app_id.to_string(), owner.to_ascii_lowercase(), scope))
        .or_insert(until);
    if until > *held {
        *held = until;
    }
    *held
}

/// The scoped withdrawals that are about **budget** (a primary or secondary
/// rate limit), as opposed to a refused credential. Same key and end as the
/// entry [`withdraw_scoped_budget_until`] also writes to the scoped table.
fn budget_withdrawals() -> &'static Mutex<HashMap<ScopedKey, SystemTime>> {
    static BUDGET: OnceLock<Mutex<HashMap<ScopedKey, SystemTime>>> = OnceLock::new();
    BUDGET.get_or_init(|| Mutex::new(HashMap::new()))
}

/// [`withdraw_scoped_until`] for a rate limit: the reader is out of
/// **budget** for `owner`'s `scope`, which is the one reason a deferrable
/// read may be shed (W4-C) rather than sent to the writer. Returns the
/// instant the scoped withdrawal now ends.
pub fn withdraw_scoped_budget_until(
    app_id: &str,
    owner: &str,
    scope: ResourceScope,
    until: SystemTime,
) -> SystemTime {
    let held = withdraw_scoped_until(app_id, owner, scope, until);
    if let Ok(mut map) = budget_withdrawals().lock() {
        let slot = map
            .entry((app_id.to_string(), owner.to_ascii_lowercase(), scope))
            .or_insert(held);
        if held > *slot {
            *slot = held;
        }
    }
    held
}

/// Whether reader `app_id` is withdrawn from `owner`'s `resource` at `now`
/// because of a rate limit (that resource's or [`ResourceScope::All`]'s) —
/// not a credential failure, a coverage miss or a stale token.
#[must_use]
pub fn is_budget_withdrawn_at(
    app_id: &str,
    owner: &str,
    resource: Resource,
    now: SystemTime,
) -> bool {
    let Ok(map) = budget_withdrawals().lock() else {
        return false; // unknown cause: never shed on it
    };
    let owner = owner.to_ascii_lowercase();
    [ResourceScope::of(resource), ResourceScope::All]
        .into_iter()
        .any(|scope| {
            map.get(&(app_id.to_string(), owner.clone(), scope))
                .is_some_and(|&until| now < until)
        })
}

/// Whether reader `app_id` is withdrawn from `owner`'s `resource` at `now`,
/// by a withdrawal of that resource or of [`ResourceScope::All`].
#[must_use]
pub fn is_withdrawn_scoped_at(
    app_id: &str,
    owner: &str,
    resource: Resource,
    now: SystemTime,
) -> bool {
    let Ok(map) = scoped_withdrawals().lock() else {
        return false; // fail open, as is_withdrawn_at
    };
    let owner = owner.to_ascii_lowercase();
    [ResourceScope::of(resource), ResourceScope::All]
        .into_iter()
        .any(|scope| {
            map.get(&(app_id.to_string(), owner.clone(), scope))
                .is_some_and(|&until| now < until)
        })
}

/// When reader `app_id`'s App-wide withdrawal ends, if it is withdrawn at
/// `now` (W4-B: the instant an exhausted route can expect it back).
#[must_use]
pub fn withdrawn_until(app_id: &str, now: SystemTime) -> Option<SystemTime> {
    let map = withdrawals().lock().ok()?;
    map.get(app_id).copied().filter(|&until| now < until)
}

/// When reader `app_id`'s scoped withdrawal from `owner`'s `resource` ends
/// (the later of that resource's and [`ResourceScope::All`]'s), if one is
/// live at `now` (W4-B: a spill latch entered on a withdrawal releases no
/// earlier than this).
#[must_use]
pub fn scoped_withdrawal_until(
    app_id: &str,
    owner: &str,
    resource: Resource,
    now: SystemTime,
) -> Option<SystemTime> {
    let map = scoped_withdrawals().lock().ok()?;
    let owner = owner.to_ascii_lowercase();
    [ResourceScope::of(resource), ResourceScope::All]
        .into_iter()
        .filter_map(|scope| {
            map.get(&(app_id.to_string(), owner.clone(), scope))
                .copied()
        })
        .filter(|&until| now < until)
        .max()
}

/// Every scoped withdrawal still live at `now`, as `(app id, owner, scope,
/// until)` in key order — what `loom-daemon status` lists.
#[must_use]
pub fn live_scoped_withdrawals(
    now: SystemTime,
) -> Vec<(String, String, ResourceScope, SystemTime)> {
    let Ok(map) = scoped_withdrawals().lock() else {
        return Vec::new();
    };
    let mut out: Vec<_> = map
        .iter()
        .filter(|(_, &until)| now < until)
        .map(|((app, owner, scope), &until)| (app.clone(), owner.clone(), *scope, until))
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    out
}

// ---------------------------------------------------------------------------
// Credentials keyed by (owner, app)
// ---------------------------------------------------------------------------

/// The `GH_CONFIG_DIR` carrying `owner`'s token minted from `app_id`.
///
/// Nested **under** the existing per-owner directory
/// (`gh-config-by-owner/<owner>/<app-id>`) rather than beside it, so the
/// single-App layout (`gh-config-by-owner/<owner>`) keeps its exact meaning and
/// a fleet with no pool sees no new directories at all.
#[must_use]
pub fn gh_config_dir_for_owner_app(workspace_root: &Path, owner: &str, app_id: &str) -> PathBuf {
    crate::credential_preflight::github_app_gh_config_dir_for_owner(workspace_root, owner)
        .join(app_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(ids: &[&str]) -> Vec<PoolMember> {
        ids.iter()
            .map(|id| PoolMember {
                app_id: (*id).to_string(),
                private_key_path: PathBuf::from(format!("/keys/{id}.pem")),
            })
            .collect()
    }

    #[test]
    fn an_empty_pool_selects_nothing() {
        assert_eq!(select_for_repo(&[], "2AMLogic/2am"), None);
        assert_eq!(assignment_index("2AMLogic/2am", 0), None);
    }

    #[test]
    fn assignment_matches_the_cross_host_golden_table() {
        // These literals are the cross-host contract: if any value here ever
        // changes, two daemons on different loom versions disagree about
        // which member serves a repo. Independently computed in Python
        // (SHA-256 of "loom-read-pool/v1:<repo>", first 8 bytes big-endian,
        // mod N) against the spec in `assignment_index`'s doc comment, not by
        // calling this function — a golden table computed by calling the
        // function it guards can't catch a regression in that function.
        //
        // N=3 alone can't guard the byte order in `assignment_index`'s
        // `u64::from_be_bytes`: since 256 ≡ 1 (mod 3), the mod-3 result only
        // depends on the byte *sum*, which an endianness swap leaves
        // unchanged. N=4 does not have that property and is included so a
        // `from_be_bytes` -> `from_le_bytes` regression fails this test.
        let golden: &[(&str, usize, usize)] = &[
            // (owner_repo, N=3, N=4)
            ("2AMLogic/2am", 2, 2),
            ("rjwalters/loom", 0, 3),
            ("2AMLogic/sigchip", 1, 2),
            ("2AMLogic/marketing", 0, 1),
        ];
        for (repo, want_n3, want_n4) in golden {
            assert_eq!(assignment_index(repo, 3), Some(*want_n3), "N=3 mismatch for {repo}");
            assert_eq!(assignment_index(repo, 4), Some(*want_n4), "N=4 mismatch for {repo}");
        }
    }

    #[test]
    fn assignment_at_n2_puts_loom_on_the_second_reader() {
        // W4-B golden row, computed independently in Python:
        // SHA-256("loom-read-pool/v1:rjwalters/loom")[..8] BE mod 2 = 1.
        assert_eq!(assignment_index("rjwalters/loom", 2), Some(1));
    }

    #[test]
    fn split_and_spill_indices_match_their_golden_tables() {
        // W4-B cross-host contract, computed independently in Python
        // (hashlib.sha256(prefix + owner_repo.lower() + "|" + key), first 8
        // bytes big-endian, mod 4), not by calling these functions. N=4 guards
        // the byte order: every row's little-endian answer differs from at
        // least one of the two columns (split rows 1 and 3; spill all four).
        let unit = "\u{1f}";
        let golden: &[(&str, String, usize, usize)] = &[
            // (owner_repo, affinity_key, split N=4, spill N=4)
            ("acme/hot", format!("api{unit}repos/acme/hot/issues/1"), 0, 2),
            ("acme/hot", "repos/acme/hot/issues?state=open".to_string(), 1, 2),
            ("rjwalters/loom", "repos/rjwalters/loom/pulls/42".to_string(), 0, 1),
            ("acme/widgets", format!("api{unit}repos/acme/widgets/commits"), 3, 0),
        ];
        for (repo, key, split, spill) in golden {
            assert_eq!(split_index(repo, key, 4), Some(*split), "split {repo} {key:?}");
            assert_eq!(spill_index(repo, key, 4), Some(*spill), "spill {repo} {key:?}");
            // The owner/repo is lowercased before hashing.
            assert_eq!(split_index(&repo.to_uppercase(), key, 4), Some(*split));
        }
        assert_eq!(split_index("acme/hot", "k", 0), None);
        assert_eq!(spill_index("acme/hot", "k", 0), None);
    }

    #[test]
    fn the_walk_starts_at_start_and_wraps() {
        assert_eq!(walk_order(2, 4).collect::<Vec<_>>(), vec![2, 3, 0, 1]);
        assert_eq!(walk_order(0, 0).count(), 0);
    }

    #[test]
    fn assignment_is_stable_for_the_same_repo_and_pool_size() {
        let first = assignment_index("2AMLogic/2am", 3);
        assert_eq!(first, assignment_index("2AMLogic/2am", 3));
        assert!(first.unwrap() < 3);
    }

    #[test]
    fn assignment_spreads_repos_across_members() {
        let repos = [
            "2AMLogic/2am",
            "rjwalters/loom",
            "2AMLogic/sigchip",
            "2AMLogic/marketing",
            "rjwalters/anvil",
            "2AMLogic/gf180-pll",
            "2AMLogic/klayout-tools",
            "rjwalters/safehouse",
        ];
        let seen: std::collections::HashSet<_> = repos
            .iter()
            .filter_map(|r| assignment_index(r, 3))
            .collect();
        // Not a distribution test — just that it is not degenerate.
        assert!(seen.len() > 1, "every repo hashed to the same member: {seen:?}");
    }

    // The withdrawal registry is process-global and `cargo test` runs these in
    // parallel threads of ONE process, so every test below uses app ids unique
    // to itself. A shared "clear the map" helper would be the obvious
    // alternative and is the wrong one: it makes each test's correctness
    // depend on no sibling running concurrently.

    #[test]
    fn a_withdrawn_member_is_skipped_deterministically() {
        let members = pool(&["skip-a", "skip-b", "skip-c"]);
        let repo = "2AMLogic/2am";
        let chosen = select_for_repo(&members, repo).unwrap().app_id.clone();

        withdraw(&chosen);
        let replacement = select_for_repo(&members, repo).unwrap();
        assert_ne!(replacement.app_id, chosen);
        // Deterministic fallback: the same replacement every time, so two
        // hosts seeing the same withdrawal agree.
        assert_eq!(select_for_repo(&members, repo).unwrap().app_id, replacement.app_id);
    }

    #[test]
    fn every_member_withdrawn_selects_nothing() {
        let members = pool(&["all-a", "all-b"]);
        withdraw("all-a");
        withdraw("all-b");
        assert_eq!(select_for_repo(&members, "2AMLogic/2am"), None);
    }

    #[test]
    fn a_withdrawal_expires() {
        let past = SystemTime::now() - Duration::from_secs(60);
        withdraw_until("expiry-stale", past);
        assert!(!is_withdrawn("expiry-stale"));
    }

    #[test]
    fn a_withdrawal_is_extended_never_shortened() {
        let now = SystemTime::now();
        withdraw_until("extend-x", now + Duration::from_secs(600));
        withdraw_until("extend-x", now + Duration::from_secs(10));
        assert!(is_withdrawn_at("extend-x", now + Duration::from_secs(300)));
    }

    #[test]
    fn config_pool_parses_both_app_id_spellings() {
        let value: Value = serde_json::from_str(
            r#"[{"appId":"5100879","privateKeyPath":"/k/a.pem"},
                {"appId":5101048,"privateKeyPath":"/k/b.pem"}]"#,
        )
        .unwrap();
        let members: Vec<_> = value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(member_from_value)
            .collect();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].app_id, "5100879");
        assert_eq!(members[1].app_id, "5101048");
        assert_eq!(members[1].private_key_path, PathBuf::from("/k/b.pem"));
    }

    #[test]
    fn a_malformed_config_entry_is_skipped_not_fatal() {
        let value: Value = serde_json::from_str(
            r#"[{"appId":"","privateKeyPath":"/k/a.pem"},
                {"privateKeyPath":"/k/b.pem"},
                {"appId":"5101048"},
                {"appId":"5101048","privateKeyPath":"/k/b.pem"}]"#,
        )
        .unwrap();
        let members: Vec<_> = value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(member_from_value)
            .collect();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].app_id, "5101048");
    }

    #[test]
    fn the_credential_dir_nests_under_the_existing_per_owner_dir() {
        let root = Path::new("/w");
        let owner_dir =
            crate::credential_preflight::github_app_gh_config_dir_for_owner(root, "2AMLogic");
        assert_eq!(
            gh_config_dir_for_owner_app(root, "2AMLogic", "5100879"),
            owner_dir.join("5100879"),
        );
    }
}
