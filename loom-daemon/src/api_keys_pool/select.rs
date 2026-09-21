//! Spawn-time account selection for the API-key pool (issue #8401).
//!
//! The ladder mirrors `spawn-claude.sh`'s / [`crate::tokens_pool::select`]'s
//! shape, minus the tiers that have no data source yet in this slice:
//!
//! | Tier | Claude OAuth pool | API-key pool (this slice) |
//! |---|---|---|
//! | 1 | `.ranking` health order | *not yet* — see the exhaustion-state follow-up |
//! | 2 | `.allowlist` operator pin | `.allowlist` operator pin |
//! | 3 | rotation cursor / random | rotation cursor (round-robin) |
//!
//! Tier 1 is a deliberate, documented gap rather than a stub: until the
//! exhaustion classifier exists there is nothing truthful to rank on, and an
//! invented ordering would be worse than round-robin. The tier-2/3 code below
//! is already written against a filtered candidate set, so adding a health
//! filter later is an additional filter, not a restructure.
//!
//! Failure is **fail-closed at exit 78** (`EX_CONFIG`), the same code and
//! operator-diagnostic shape an empty Claude pool produces — but only once a
//! provider is actually pooled on this host. A host with no pool at all keeps
//! today's behaviour (inherit the launching environment, or let the harness use
//! its own auth store), so the #8363 single-key trial path is unchanged.
//!
//! "No pool" means the provider directory **does not exist** (or holds no
//! account file). A directory that exists but cannot be read is the opposite
//! case and is an error on every path here — see
//! [`super::paths::PoolReadError`]. The same goes for the state files that
//! decide eligibility: an unreadable `.disabled`/`.allowlist` or an unparsable
//! `.bad_accounts.json` withholds accounts, it never frees them.

use std::path::{Path, PathBuf};

use crate::tokens_pool::{rng::Rng, rotation::next_rotation_index};

use super::paths::{list_workspace_providers, provider_dir, resolve_provider_root, PoolReadError};
use super::registry::{
    list_provider, provider_is_pooled, read_credential, ApiKeyAccount, Credential, Ineligible,
    ALLOWLIST_FILE,
};

/// Exit code when no account is available (sysexits.h `EX_CONFIG`), identical
/// to [`crate::tokens_pool::select::EX_CONFIG`].
pub const EX_CONFIG: i32 = 78;

/// A selected account plus its credential. No `Serialize`; `Debug` redacts.
#[derive(Clone)]
pub struct SelectedApiKey {
    pub provider: String,
    /// The account **name** — this is what may be recorded in a launch record,
    /// a journal, or a log line.
    pub name: String,
    pub credential: Credential,
    pub path: PathBuf,
}

impl std::fmt::Debug for SelectedApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedApiKey")
            .field("provider", &self.provider)
            .field("name", &self.name)
            .field("credential", &self.credential)
            .field("path", &self.path)
            .finish()
    }
}

/// Every registered account for this provider is unusable right now.
#[derive(Debug)]
pub struct EmptyApiKeyPoolError(pub String);

impl std::fmt::Display for EmptyApiKeyPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EmptyApiKeyPoolError {}

/// `true` when `workspace`'s effective pool holds at least one account for
/// `provider` — the signal that pool selection, and therefore fail-closed
/// behaviour, applies at all.
///
/// # Errors
/// [`PoolReadError`] when a pool directory exists but cannot be read. The spawn
/// path must turn that into exit 78, **not** into "unpooled".
pub fn is_pooled(workspace: &Path, provider: &str) -> Result<bool, PoolReadError> {
    provider_is_pooled(&resolve_provider_root(workspace, provider)?, provider)
}

/// Select one account for `provider` from `workspace`'s effective pool.
pub fn select_api_key(
    workspace: &Path,
    provider: &str,
    rng: Option<&mut Rng>,
) -> Result<SelectedApiKey, EmptyApiKeyPoolError> {
    select_api_key_for(workspace, provider, None, rng)
}

/// [`select_api_key`] for a caller that knows which variable the credential is
/// *for* (a profile's `credentialEnv`). An account whose file assigns a
/// different variable is withheld: the pool namespace is only a directory
/// name, so without this check a key registered under the wrong namespace
/// would be injected under this profile's harness variable and sent to the
/// other provider's endpoint.
pub fn select_api_key_for(
    workspace: &Path,
    provider: &str,
    expected_env: Option<&str>,
    rng: Option<&mut Rng>,
) -> Result<SelectedApiKey, EmptyApiKeyPoolError> {
    let unreadable = |e: PoolReadError| EmptyApiKeyPoolError(e.to_string());
    let root = resolve_provider_root(workspace, provider).map_err(unreadable)?;
    let mut accounts = list_provider(&root, provider).map_err(unreadable)?;
    if let Some(expected) = expected_env {
        for account in &mut accounts {
            let Some(actual) = account.env_name.as_deref().filter(|a| *a != expected) else {
                continue;
            };
            // Both names passed the strict `UPPER_SNAKE_CASE` check, so
            // neither can be key material.
            account.problem = Some(format!(
                "assigns {actual}, but the profile's credentialEnv is {expected}; register it \
                 with `--env-var {expected}`"
            ));
            account.ineligible = Some(Ineligible::Malformed);
        }
    }
    if accounts.is_empty() {
        return Err(EmptyApiKeyPoolError(format!(
            "No API-key accounts registered for provider {provider:?} in {}. Register one with \
             `loom-daemon api-keys add {provider} <name> --key-file <path>`.",
            root.display()
        )));
    }

    let mut owned_rng;
    let rng: &mut Rng = match rng {
        Some(r) => r,
        None => {
            owned_rng = Rng::from_entropy();
            &mut owned_rng
        }
    };

    let eligible: Vec<&ApiKeyAccount> = accounts.iter().filter(|a| a.selectable()).collect();
    if eligible.is_empty() {
        return Err(EmptyApiKeyPoolError(exhausted_detail(provider, &root, &accounts)));
    }

    // Tier 2: operator pin. An empty intersection is a stale pin, not a reason
    // to strand the spawner — fall through to the full eligible set, the same
    // fail-safe the Claude pool applies to stale advisory exclusions.
    // An allowlist that exists but cannot be read is not a stale pin — the
    // operator's restriction is unknown, so do not guess past it.
    let pinned = super::registry::read_list(&root, provider, ALLOWLIST_FILE)
        .map_err(|e| EmptyApiKeyPoolError(format!("operator pin is unreadable: {e}")))?;
    let mut candidates: Vec<&ApiKeyAccount> = if pinned.is_empty() {
        eligible.clone()
    } else {
        let intersection: Vec<&ApiKeyAccount> = eligible
            .iter()
            .copied()
            .filter(|a| pinned.contains(&a.name))
            .collect();
        if intersection.is_empty() {
            eligible.clone()
        } else {
            intersection
        }
    };
    candidates.sort_by(|a, b| a.name.cmp(&b.name));

    // Tier 3: one-per-account round-robin across concurrent dispatches, reusing
    // the Claude pool's cursor implementation so both pools spread the same way.
    let dir = provider_dir(&root, provider);
    let index = next_rotation_index(&dir, candidates.len(), rng);
    let chosen = candidates[index.min(candidates.len() - 1)];

    let credential = read_credential(&root, provider, &chosen.name)
        .map_err(|e| EmptyApiKeyPoolError(format!("selected account became unreadable: {e}")))?;
    Ok(SelectedApiKey {
        provider: provider.to_string(),
        name: chosen.name.clone(),
        credential,
        path: chosen.path.clone(),
    })
}

/// Per-account "why not" detail, mirroring the Claude pool's empty-pool error
/// (#4643): say which accounts exist, why each was excluded, and which binary
/// decided, so a recurrence is diagnosable from the log alone.
fn exhausted_detail(provider: &str, root: &Path, accounts: &[ApiKeyAccount]) -> String {
    let detail: String = accounts
        .iter()
        .map(|a| {
            let reason = match &a.ineligible {
                Some(Ineligible::Disabled) => {
                    format!(
                        "disabled by operator — `loom-daemon api-keys enable {provider} {}`",
                        a.name
                    )
                }
                Some(Ineligible::Malformed) => format!(
                    "unusable file ({}) — re-register with `loom-daemon api-keys add {provider} {} \
                     --key-file <path> --force`",
                    a.problem.as_deref().unwrap_or("malformed"),
                    a.name
                ),
                Some(Ineligible::Exhausted) => format!(
                    "{} — clears on its own, or `loom-daemon api-keys unblock {provider} {}`",
                    a.problem.as_deref().unwrap_or("bad-marked"),
                    a.name
                ),
                Some(Ineligible::Unverifiable) => format!(
                    "withheld, pool state unreadable — {}",
                    a.problem.as_deref().unwrap_or("state file unreadable")
                ),
                None => "eligible".to_string(),
            };
            format!("\n  - {}: {reason}", a.name)
        })
        .collect();
    format!(
        "All {} API-key account(s) for provider {provider:?} in {} are disabled or unusable.\
         {detail}\n  deciding binary: {}\n  \
         Inspect with `loom-daemon api-keys health --provider {provider}`.",
        accounts.len(),
        root.display(),
        crate::tokens_pool::select::deciding_binary_identity(),
    )
}

/// Secret-free health snapshot for one provider.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderHealth {
    pub provider: String,
    pub dir: PathBuf,
    pub total: usize,
    pub selectable: usize,
    pub disabled: usize,
    pub malformed: usize,
    /// Accounts with an active [`super::bad_marks`] entry (exhausted/rate-
    /// limited, until their reset horizon).
    pub exhausted: usize,
    /// Accounts withheld because `.disabled` / `.bad_accounts.json` exists but
    /// cannot be read or parsed.
    pub unverifiable: usize,
    /// Set when the provider directory itself exists but cannot be read. Every
    /// count above is then `0` and means "unknown", not "none".
    pub unreadable: Option<String>,
    /// Accounts whose file permissions are looser than `0600`.
    pub insecure_permissions: Vec<String>,
    pub pinned: Vec<String>,
    pub accounts: Vec<ApiKeyAccount>,
}

/// Every registered account visible from `workspace` (or just `provider`'s),
/// each provider read from its own effective root.
///
/// # Errors
/// [`PoolReadError`] when any pool directory involved exists but cannot be
/// read — `list` must say so rather than print "no accounts registered".
pub fn list_accounts(
    workspace: &Path,
    provider: Option<&str>,
) -> Result<Vec<ApiKeyAccount>, PoolReadError> {
    let providers = match provider {
        Some(p) => vec![p.to_string()],
        None => list_workspace_providers(workspace)?,
    };
    let mut accounts = Vec::new();
    for p in &providers {
        accounts.extend(list_provider(&resolve_provider_root(workspace, p)?, p)?);
    }
    Ok(accounts)
}

/// Snapshot `provider`'s pool health, or every pooled provider when `provider`
/// is `None`. An unreadable provider directory is reported in its own entry
/// ([`ProviderHealth::unreadable`]) so it cannot hide the healthy ones.
///
/// # Errors
/// [`PoolReadError`] when a pool *root* cannot be enumerated at all.
pub fn health(
    workspace: &Path,
    provider: Option<&str>,
) -> Result<Vec<ProviderHealth>, PoolReadError> {
    let providers = match provider {
        Some(p) => vec![p.to_string()],
        None => list_workspace_providers(workspace)?,
    };
    Ok(providers
        .into_iter()
        .map(|provider| {
            let resolved = resolve_provider_root(workspace, &provider)
                .and_then(|root| list_provider(&root, &provider).map(|accounts| (root, accounts)));
            let (root, accounts, unreadable) = match resolved {
                Ok((root, accounts)) => (root, accounts, None),
                Err(e) => (
                    e.path
                        .parent()
                        .map_or_else(|| e.path.clone(), Path::to_path_buf),
                    Vec::new(),
                    Some(e.to_string()),
                ),
            };
            let count = |kind: Ineligible| {
                accounts
                    .iter()
                    .filter(|a| a.ineligible.as_ref() == Some(&kind))
                    .count()
            };
            ProviderHealth {
                dir: provider_dir(&root, &provider),
                total: accounts.len(),
                selectable: accounts.iter().filter(|a| a.selectable()).count(),
                disabled: count(Ineligible::Disabled),
                malformed: count(Ineligible::Malformed),
                exhausted: count(Ineligible::Exhausted),
                unverifiable: count(Ineligible::Unverifiable),
                unreadable,
                insecure_permissions: accounts
                    .iter()
                    .filter(|a| !a.permissions_ok)
                    .map(|a| a.name.clone())
                    .collect(),
                pinned: super::registry::read_list(&root, &provider, ALLOWLIST_FILE)
                    .unwrap_or_default(),
                accounts,
                provider,
            }
        })
        .collect())
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
