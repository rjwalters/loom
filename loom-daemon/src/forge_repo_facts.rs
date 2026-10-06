//! Repository facts without a forge call per use: which repo a checkout's
//! `gh` call lands on, and who owns it now.
//!
//! # Why
//!
//! Several hot paths asked the forge the same static question on every pass:
//! `gh api repos/{owner}/{repo} --jq .owner.login` (the worktree and
//! primary-checkout reapers, `landed`, `clean`), and `gh repo view` (the
//! open-linked-PR probe, the dispatch guards, the telemetry collector). The
//! answer changes only on a rename, a transfer or a remote edit.
//!
//! # Three layers
//!
//! 1. **The base repo** ([`base_repo`]) — resolved locally exactly as gh
//!    resolves it ([`crate::write_scope::target::gh_target`]), zero forge
//!    calls, memoised until any git config file that defines it changes
//!    ([`ConfigFp`]). Each caller states its `GH_REPO` rule ([`GhRepoEnv`]).
//!    A root whose local answer may differ from gh's (`ambiguous`) is
//!    cross-checked once per fingerprint against gh itself; on disagreement
//!    the root keeps its legacy forge calls for the life of the process.
//! 2. **The canonical record** ([`canonical`]) — the post-redirect
//!    owner/name, refreshed by a conditional `GET repos/<nwo>` at most every
//!    [`verify_ttl_secs`] (and for free by matching first-hand responses,
//!    [`observe`]). A failed read backs off [`SUSPECT_BACKOFF_SECS`].
//!
//! # Safety
//!
//! A remembered owner can be stale, and a stale owner turns "no PR" into a
//! false negative. So every *verified negative* built from a fact — the
//! reapers' `NoPr`, the linked-PR probe's `NoneOpen` — is confirmed against
//! the forge first ([`confirm_owner`], at most once per root per
//! [`PassScope`]); a failed or disagreeing confirm is `Unknown`, never a
//! negative. An unresolvable fact is [`Lookup::Unavailable`], which every
//! caller maps to its fail-closed path. Where `None` would fail OPEN instead
//! (the sweep registry's guard resolver, whose guards skip on a missing
//! repo) or merely drop data (the telemetry collector), `Unavailable` is
//! treated like `Legacy`: last-known answer, else the pre-facts call.
//!
//! 3. **The installation snapshot** ([`installation`], W8) — per credential,
//!    one conditional `GET installation/repositories` listing `id`,
//!    `full_name` and `private` for every repo the installation reaches,
//!    revalidated hourly. Telemetry visibility, the D32 repo identity and the
//!    write-scope probe read it instead of one `GET repos/<nwo>` per repo. A
//!    missing, stale or failed answer is never "public".
//!
//! # Kill switch
//!
//! `LOOM_REPO_FACTS=0` makes every migrated site issue exactly its previous
//! forge call ([`Lookup::Legacy`]) and restores the ETag store's
//! process-lifetime `origin` memo.

mod base;
mod confirm;
mod crosscheck;
pub(crate) mod installation;
mod record;
mod state;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_sites;

use std::path::Path;

pub(crate) use base::{base_repo, origin_identity};
pub(crate) use confirm::{confirm_owner, confirmed_in_pass, PassScope};
pub(crate) use record::{invalidate, observe};
#[cfg(test)]
pub(crate) use state::{advance_test_clock, set_test_enabled, set_test_env, test_git_forks};

/// Counter: a checkout's remote names a pre-rename / pre-transfer slug.
pub(crate) const REDIRECTED: &str = "repo_facts.redirected";
/// Counter: the local resolver and gh disagreed on an ambiguous root.
pub(crate) const RESOLVER_DISAGREE: &str = "repo_facts.resolver_disagree";
/// Default for `LOOM_REPO_FACTS_VERIFY_SECS`.
pub(crate) const VERIFY_TTL_DEFAULT_SECS: i64 = 21_600;
/// No new read of a record whose last read failed, for this long.
pub(crate) const SUSPECT_BACKOFF_SECS: i64 = 300;

/// Whether repo facts serve the migrated sites (`LOOM_REPO_FACTS` ≠ `0`).
pub(crate) fn enabled() -> bool {
    let killed = state::env_var("LOOM_REPO_FACTS").is_some_and(|v| v.trim() == "0");
    !killed && state::default_on()
}

/// Whether `root` is pinned to its legacy forge calls under `env` (its
/// local answer disagreed with gh's own): such a root's local resolution is
/// not trusted for read routing either (W4-C).
pub(crate) fn is_legacy_pinned(root: &Path, env: GhRepoEnv) -> bool {
    let pin = (root.to_path_buf(), env);
    state::with(|s| s.legacy.contains(&pin))
}

/// How long a verified record is used without a re-read
/// (`LOOM_REPO_FACTS_VERIFY_SECS`, default six hours).
pub(crate) fn verify_ttl_secs() -> i64 {
    state::env_var("LOOM_REPO_FACTS_VERIFY_SECS")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(VERIFY_TTL_DEFAULT_SECS)
}

/// Which `GH_REPO` rule the replaced command followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum GhRepoEnv {
    /// `gh api` placeholders: the facade exports `LOOM_REPO` as `GH_REPO`,
    /// else the child inherits the daemon's own `GH_REPO`.
    Honour,
    /// `gh repo view`, which ignores `GH_REPO`.
    Ignore,
}

impl GhRepoEnv {
    /// The `GH_REPO` gh would see under this rule.
    pub(crate) fn effective_gh_repo(self) -> Option<String> {
        match self {
            GhRepoEnv::Ignore => None,
            GhRepoEnv::Honour => match state::env_var("LOOM_REPO") {
                Some(v) => Some(v).filter(|v| !v.trim().is_empty()),
                None => state::env_var("GH_REPO").filter(|v| !v.trim().is_empty()),
            },
        }
    }
}

/// A usable canonical answer for a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fact {
    pub(crate) host: String,
    /// The `owner/repo` the checkout names (the record's key).
    pub(crate) configured_nwo: String,
    /// The post-redirect owner login — what `.owner.login` returned.
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) verified_at: i64,
    /// Read from the forge during this lookup (not remembered).
    pub(crate) fresh: bool,
}

impl Fact {
    pub(crate) fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// What a migrated site does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// Use this answer.
    Fact(Fact),
    /// Issue the site's own pre-facts forge call: facts are off, the root has
    /// no locally resolvable repo, or the root is pinned to legacy.
    Legacy,
    /// The fact cannot be established right now: the site's fail-closed path
    /// (`None`, `ProbeFailed`, a GraphQL fallback), or — where `None` would
    /// fail open — the site's last-known answer, else its legacy call.
    Unavailable,
}

/// [`canonical_with`] under the default `gh` resolver.
pub(crate) fn canonical(root: &Path, env: GhRepoEnv) -> Lookup {
    canonical_with(Path::new("gh"), root, env)
}

/// The canonical repo for `root` under `env`, reading the forge only when the
/// record is missing, expired or suspect (and not in its failure backoff).
pub(crate) fn canonical_with(gh: &Path, root: &Path, env: GhRepoEnv) -> Lookup {
    if !enabled() {
        return Lookup::Legacy;
    }
    let Some(base) = base_repo(root, env) else {
        return Lookup::Legacy;
    };
    let pin = (root.to_path_buf(), env);
    if state::with(|s| s.legacy.contains(&pin)) {
        return Lookup::Legacy;
    }
    let key = record::record_key(&base.host, &base.nwo);
    let now = state::now();
    let prior = record::load(&key);
    let (rec, fresh) = match prior {
        Some(r) if r.usable(now, verify_ttl_secs()) => (r, false),
        Some(r) if r.in_backoff(now) => return Lookup::Unavailable,
        prior => match record::verify(gh, root, &base, prior.as_ref(), "repo_facts.verify") {
            Ok(r) => (r, true),
            Err(_) => return Lookup::Unavailable,
        },
    };
    let fact = Fact {
        host: base.host.clone(),
        configured_nwo: base.nwo.clone(),
        owner: rec.canonical_owner.clone(),
        name: rec.canonical_name.clone(),
        verified_at: rec.verified_at,
        fresh,
    };
    warn_if_redirected(root, &fact);
    if base.ambiguous {
        let checked = (root.to_path_buf(), env, base.fp.clone());
        if !state::with(|s| s.crosschecked.contains(&checked)) {
            let Some(theirs) = crosscheck::gh_answer(gh, root, env) else {
                // gh could not answer either: this call keeps its legacy
                // path, and the check is retried on the next use.
                return Lookup::Legacy;
            };
            state::with(|s| s.crosschecked.insert(checked));
            if !theirs.eq_ignore_ascii_case(&fact.full_name()) {
                let n = crate::forge_call_stats::counters::bump(RESOLVER_DISAGREE);
                log::warn!(
                    "forge_repo_facts: {} resolves to {} ({}) locally but gh says {theirs}; \
                     this root keeps its legacy forge calls ({RESOLVER_DISAGREE}={n})",
                    root.display(),
                    fact.full_name(),
                    base.via
                );
                state::with(|s| s.legacy.insert(pin));
                return Lookup::Legacy;
            }
        }
    }
    Lookup::Fact(fact)
}

/// Warn once per `(root, canonical)` when the remote names a pre-redirect
/// slug, and count it.
fn warn_if_redirected(root: &Path, fact: &Fact) {
    let canonical = fact.full_name();
    if canonical.eq_ignore_ascii_case(&fact.configured_nwo) {
        return;
    }
    let first = state::with(|s| s.warned.insert((root.to_path_buf(), canonical.clone())));
    if first {
        let n = crate::forge_call_stats::counters::bump(REDIRECTED);
        log::warn!(
            "forge_repo_facts: {}: the remote names {}, the forge says {canonical} (renamed or \
             transferred; update the remote with `git remote set-url`) ({REDIRECTED}={n})",
            root.display(),
            fact.configured_nwo
        );
    }
}

/// The owner a placeholder (`gh api repos/{owner}/{repo}`) call would see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnerFact {
    pub(crate) owner: String,
    /// When the forge last confirmed it; `None` for a legacy live read.
    pub(crate) verified_at: Option<i64>,
    /// The fact it came from; `None` = the legacy call answered, and every
    /// downstream check keeps its legacy behaviour.
    pub(crate) fact: Option<Fact>,
}

impl OwnerFact {
    pub(crate) fn from_fact(fact: Fact) -> Self {
        Self {
            owner: fact.owner.clone(),
            verified_at: Some(fact.verified_at),
            fact: Some(fact),
        }
    }

    pub(crate) fn legacy(owner: String) -> Self {
        Self {
            owner,
            verified_at: None,
            fact: None,
        }
    }
}

/// Where a root's three notions of "its repo" disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Disagreement {
    pub(crate) origin_nwo: Option<String>,
    pub(crate) gh_base_nwo: String,
    pub(crate) canonical_nwo: Option<String>,
}

/// `Some` when `root`'s origin, gh's base repo (placeholder semantics) and
/// the recorded canonical repo are not all the same. Local only: no forge
/// call. For read routing that keys on `origin` to see when gh would act on
/// a different repo.
#[allow(dead_code)] // the read-routing consumer lands separately; tested here
pub(crate) fn disagreement(root: &Path) -> Option<Disagreement> {
    let base = base_repo(root, GhRepoEnv::Honour)?;
    let canonical = record::load(&record::record_key(&base.host, &base.nwo))
        .filter(|r| !r.canonical_owner.is_empty())
        .map(|r| r.full_name());
    let same = |a: &str| a.eq_ignore_ascii_case(&base.nwo);
    let differs = !base.origin_nwo.as_deref().is_some_and(same)
        || canonical.as_deref().is_some_and(|c| !same(c));
    differs.then(|| Disagreement {
        origin_nwo: base.origin_nwo.clone(),
        gh_base_nwo: base.nwo.clone(),
        canonical_nwo: canonical,
    })
}

/// `owner/name` from a REST `repository_url`
/// (`https://api.github.com/repos/OWNER/NAME`).
pub(crate) fn full_name_from_repository_url(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("/repos/")?;
    let mut segs = rest.trim_end_matches('/').split('/');
    let (owner, name) = (segs.next()?, segs.next()?);
    (!owner.is_empty() && !name.is_empty() && segs.next().is_none())
        .then(|| format!("{owner}/{name}"))
}
