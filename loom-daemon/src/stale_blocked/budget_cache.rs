//! The memoised budget reading for the stale-blocked passes (#11301).
//!
//! Before it gathers, `check-stale-blocked`, `notify-cleared-blockers` and the
//! daemon's `loom:blocked` release pass ([`super::release_gh`]) read the live
//! budget ([`super::budget`]). That probe is two calls: REST `GET /rate_limit`,
//! which is free, and the GraphQL `rateLimit` query, which is charged at least
//! one point. The GraphQL leg has to stay, because `/rate_limit`'s own
//! `resources.graphql` can disagree with the bucket the GraphQL endpoint
//! enforces (#10038). The release pass visits every registered root each
//! minute and probes each root that has a `loom:blocked` item, so roots that
//! share one writer App spent one charged query per root per pass on the same
//! bucket (#11301 measured about 1,300 GraphQL requests an hour across three
//! hosts).
//!
//! So one reading answers, for [`DEFAULT_TTL`], for every root whose probe
//! runs under the same credential:
//!
//! - **Key** ([`CredentialKey`]): the `GH_CONFIG_DIR` the probe's `gh` runs
//!   under, which is the facade's own choice for the probe
//!   ([`forge::probe_config_dir`]: the explicit dir, else the root's per-owner
//!   registration, else the daemon's), plus the `gh` program. The probe is
//!   pinned to the writer identity, so it never runs as a reader App, and a
//!   reader-1 / reader-2 split never shares a reading. When the credential
//!   cannot be named that way, the key is the root itself, which is the
//!   pre-#11301 granularity. That happens when an env token reaches `gh` (it
//!   outranks any directory), when no `GH_CONFIG_DIR` resolves at all, or when
//!   a forge-egress launcher is configured (it picks a per-owner credential
//!   from the request's repository or the checkout's remote, not from the
//!   directory).
//! - **Freshness**: a reading younger than the TTL answers with no forge call,
//!   so it emits no `invoke github` span, and `budget_before` reports it
//!   `cached: true`. A probe that did not answer is not stored.
//! - **Refresh**: a closing-reference batch's own `rateLimit.remaining`, read
//!   under the same credential, overwrites the stored GraphQL figure
//!   ([`note_graphql`]), so a later root sees the lower number. It does not
//!   extend the reading's life. The core figure cannot be refreshed the same
//!   way: the run's REST reads go to a reader App first (W4-B), so their
//!   `x-ratelimit-remaining` may be that App's bucket rather than this
//!   credential's.
//!
//! Safety is unchanged. The mid-run [`super::budget::Guard`] checks every live
//! answer against the floor, so a reading that went stale inside the TTL can
//! at worst let a run start that its first live answer then stops.
//!
//! The memo is per process: each CLI invocation still probes once, and the
//! daemon's release pass is where roots share a reading. [`TTL_ENV`] overrides
//! the TTL; `0` turns the memo off, so every run probes as it did before
//! #11301.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::budget::Budget;
use crate::rate_limit_breaker::forge;
use crate::rate_limit_breaker::report::FailureContext;

/// How long one reading answers for every root on its credential.
pub const DEFAULT_TTL: Duration = Duration::from_secs(120);

/// Overrides [`DEFAULT_TTL`], in whole seconds; `0` disables the memo.
pub const TTL_ENV: &str = "LOOM_STALE_BLOCKED_BUDGET_TTL_SECS";

/// The credential a budget reading belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CredentialKey {
    /// The `GH_CONFIG_DIR` the probe's `gh` runs under.
    ConfigDir {
        dir: PathBuf,
        program: Option<String>,
    },
    /// The credential could not be named, so the reading is this root's
    /// alone.
    Root {
        root: PathBuf,
        program: Option<String>,
    },
}

/// What decides the credential a probe runs under, apart from its root and
/// program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialFacts {
    /// The effective `GH_CONFIG_DIR` ([`forge::probe_config_dir`]).
    pub config_dir: Option<PathBuf>,
    /// A `GH_TOKEN`-style variable reaches the child.
    pub env_token: bool,
    /// A forge-egress launcher is configured on this host.
    pub launcher: bool,
}

impl CredentialKey {
    /// The key for a probe run in `root` with `program`, given `facts`.
    #[must_use]
    pub fn from_facts(root: &Path, program: Option<&str>, facts: CredentialFacts) -> Self {
        let program = program.map(str::to_string);
        match facts.config_dir {
            Some(dir) if !facts.env_token && !facts.launcher => Self::ConfigDir { dir, program },
            _ => Self::Root {
                root: root.to_path_buf(),
                program,
            },
        }
    }

    /// The key for the probe `ctx` describes, from the live process state.
    #[must_use]
    pub fn for_probe(ctx: &FailureContext) -> Self {
        let facts = CredentialFacts {
            config_dir: forge::probe_config_dir(ctx),
            env_token: crate::gh_invocation::TOKEN_ENV_VARS
                .iter()
                .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty())),
            launcher: crate::gh_invocation::resolver::launcher_from_sources(
                &crate::forge_egress::policy::PolicySources::from_process(None),
            )
            .is_some(),
        };
        let root = ctx.root.clone().unwrap_or_default();
        Self::from_facts(&root, ctx.program.as_deref(), facts)
    }
}

/// One stored reading.
#[derive(Debug, Clone, Copy)]
struct Entry {
    budget: Budget,
    read_at: Instant,
}

/// Readings by credential. The process-wide memo behind [`read`] and
/// [`note_graphql`] is one of these; [`read_in`] holds the whole policy.
#[derive(Debug, Default)]
pub struct BudgetMemo {
    entries: HashMap<CredentialKey, Entry>,
}

impl BudgetMemo {
    fn fresh(&self, key: &CredentialKey, now: Instant, ttl: Duration) -> Option<Budget> {
        let entry = self.entries.get(key)?;
        let age = now.checked_duration_since(entry.read_at)?;
        (age < ttl).then_some(Budget {
            cached: true,
            ..entry.budget
        })
    }

    /// A GraphQL answer under `key`'s credential reported `remaining`: it
    /// replaces the stored figure without extending the reading's life. With
    /// nothing stored it is dropped, because a reading needs a core figure
    /// too.
    pub fn note_graphql(&mut self, key: &CredentialKey, remaining: u64) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.budget.graphql_remaining = remaining;
        }
    }
}

static MEMO: LazyLock<Mutex<BudgetMemo>> = LazyLock::new(Mutex::default);

fn lock(memo: &Mutex<BudgetMemo>) -> MutexGuard<'_, BudgetMemo> {
    memo.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The budget on `key`'s credential at `now`: the stored reading while it is
/// younger than `ttl`, else `probe`'s answer, which is stored when there is
/// one. The lock is not held across `probe`, so a slow `gh` blocks no other
/// root; two roots that miss together both probe, which is no worse than
/// before #11301.
pub fn read_in(
    memo: &Mutex<BudgetMemo>,
    key: &CredentialKey,
    now: Instant,
    ttl: Duration,
    probe: impl FnOnce() -> Option<Budget>,
) -> Option<Budget> {
    if ttl.is_zero() {
        return probe();
    }
    if let Some(hit) = lock(memo).fresh(key, now, ttl) {
        return Some(hit);
    }
    let budget = probe()?;
    lock(memo).entries.insert(
        key.clone(),
        Entry {
            budget: Budget {
                cached: false,
                ..budget
            },
            read_at: now,
        },
    );
    Some(budget)
}

/// The TTL a [`TTL_ENV`] value sets: whole seconds, else [`DEFAULT_TTL`].
#[must_use]
pub fn ttl_from(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_TTL, Duration::from_secs)
}

/// The budget for the probe `ctx` describes, whose credential is `key`: the
/// process-wide memo's reading when it is fresh, else one probe.
pub fn read(key: &CredentialKey, ctx: &FailureContext) -> Option<Budget> {
    let ttl = ttl_from(std::env::var(TTL_ENV).ok().as_deref());
    read_in(&MEMO, key, Instant::now(), ttl, || {
        forge::probe_budget_ctx(ctx, chrono::Utc::now()).map(|b| Budget {
            core_remaining: b.core_remaining,
            graphql_remaining: b.graphql_remaining,
            cached: false,
        })
    })
}

/// [`BudgetMemo::note_graphql`] on the process-wide memo.
pub fn note_graphql(key: &CredentialKey, remaining: u64) {
    lock(&MEMO).note_graphql(key, remaining);
}
