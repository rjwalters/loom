//! Provider-scoped account health for runtimes which do not have Claude's
//! quota probe. This state is deliberately separate from the legacy Claude
//! `.ranking`, `.bad_tokens`, and `.failure_counts` files.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::account_registry::{AccountDescriptor, AccountId, AccountProvider};
use super::locking::MkdirLock;

const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_EXHAUSTED_COOLDOWN_SECS: u64 = 5 * 60 * 60;
pub const DEFAULT_RECOVERABLE_BACKOFF_SECS: u64 = 60;
pub const DEFAULT_SESSION_BACKOFF_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalClassification {
    Success,
    TokenExpired,
    TokenExhausted,
    Recoverable,
    Timeout,
    Fatal,
    CwdDeleted,
    ModelRefusal,
    SessionLimit,
}

impl std::str::FromStr for TerminalClassification {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Ok(match value {
            "SUCCESS" => Self::Success,
            "TOKEN_EXPIRED" => Self::TokenExpired,
            "TOKEN_EXHAUSTED" => Self::TokenExhausted,
            "RECOVERABLE" => Self::Recoverable,
            "TIMEOUT" => Self::Timeout,
            "FATAL" => Self::Fatal,
            "CWD_DELETED" => Self::CwdDeleted,
            "MODEL_REFUSAL" => Self::ModelRefusal,
            "SESSION_LIMIT" => Self::SessionLimit,
            other => bail!("unknown terminal classification {other:?}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthReason {
    Healthy,
    ReauthRequired,
    PlanExhausted,
    TransientFailure,
    SessionLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountHealth {
    pub provider: AccountProvider,
    pub name: String,
    pub reason: HealthReason,
    pub updated_at: u64,
    pub signal_provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<u64>,
    #[serde(default)]
    pub consecutive_transient_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success: Option<u64>,
}

impl AccountHealth {
    fn id(&self) -> AccountId {
        AccountId {
            provider: self.provider,
            name: self.name.clone(),
        }
    }

    #[must_use]
    pub fn is_eligible_at(&self, now: u64) -> bool {
        self.reason != HealthReason::ReauthRequired
            && self.cooldown_until.is_none_or(|deadline| deadline <= now)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthFile {
    version: u32,
    #[serde(default)]
    accounts: Vec<AccountHealth>,
    #[serde(default)]
    cursors: HashMap<String, u64>,
}

impl Default for HealthFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            accounts: Vec::new(),
            cursors: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderCapacity {
    pub provider: AccountProvider,
    pub raw: usize,
    pub enabled: usize,
    pub healthy: usize,
    pub cooldown: usize,
    pub reauth_required: usize,
    pub observed_at: u64,
}

#[derive(Debug, Clone)]
pub struct NoHealthyAccountError {
    pub provider: AccountProvider,
    pub reasons: Vec<String>,
}

impl std::fmt::Display for NoHealthyAccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no healthy {:?} account is available: {}",
            self.provider,
            self.reasons.join(", ")
        )
    }
}

impl std::error::Error for NoHealthyAccountError {}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("account-health.json")
}

fn lock_path(workspace: &Path) -> PathBuf {
    workspace.join(".loom").join("account-health.lock")
}

fn read_state(workspace: &Path) -> Result<HealthFile> {
    let path = state_path(workspace);
    if !path.exists() {
        return Ok(HealthFile::default());
    }
    let state: HealthFile = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("malformed provider health state {}", path.display()))?;
    if state.version != SCHEMA_VERSION {
        bail!(
            "unsupported provider health schema version {} in {}",
            state.version,
            path.display()
        );
    }
    let mut ids = HashSet::new();
    for account in &state.accounts {
        if account.name.is_empty() || !ids.insert(account.id()) {
            bail!("invalid or duplicate account in provider health state");
        }
    }
    Ok(state)
}

fn write_state(workspace: &Path, state: &HealthFile) -> Result<()> {
    let path = state_path(workspace);
    let parent = path.parent().expect("health path has parent");
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".account-health.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state)?;
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

fn with_state<T>(
    workspace: &Path,
    operation: impl FnOnce(&mut HealthFile) -> Result<T>,
) -> Result<T> {
    fs::create_dir_all(workspace.join(".loom"))?;
    let _lock = MkdirLock::acquire(&lock_path(workspace)).map_err(anyhow::Error::msg)?;
    let mut state = read_state(workspace)?;
    let result = operation(&mut state)?;
    write_state(workspace, &state)?;
    Ok(result)
}

fn cooldown_from_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

pub fn record_terminal(
    workspace: &Path,
    id: &AccountId,
    classification: TerminalClassification,
    provenance: &str,
) -> Result<()> {
    record_terminal_at(workspace, id, classification, provenance, now_epoch())
}

pub fn record_terminal_at(
    workspace: &Path,
    id: &AccountId,
    classification: TerminalClassification,
    provenance: &str,
    now: u64,
) -> Result<()> {
    if id.name.is_empty() || provenance.is_empty() {
        bail!("account identity and signal provenance are required");
    }
    with_state(workspace, |state| {
        let existing = state.accounts.iter().position(|entry| entry.id() == *id);
        if matches!(
            classification,
            TerminalClassification::Timeout
                | TerminalClassification::Fatal
                | TerminalClassification::CwdDeleted
                | TerminalClassification::ModelRefusal
        ) {
            return Ok(());
        }
        let entry = existing.map_or_else(
            || AccountHealth {
                provider: id.provider,
                name: id.name.clone(),
                reason: HealthReason::Healthy,
                updated_at: now,
                signal_provenance: provenance.to_string(),
                cooldown_until: None,
                consecutive_transient_failures: 0,
                last_success: None,
            },
            |index| state.accounts.remove(index),
        );
        let mut entry = entry;
        entry.updated_at = now;
        entry.signal_provenance = provenance.to_string();
        match classification {
            // Once authentication has expired, ordinary runtime feedback cannot
            // verify that credentials were repaired. Keep the hold sticky until
            // clear_reauth() performs that explicit transition.
            _ if entry.reason == HealthReason::ReauthRequired => {
                entry.cooldown_until = None;
                if classification == TerminalClassification::Success {
                    entry.last_success = Some(now);
                    entry.consecutive_transient_failures = 0;
                }
            }
            TerminalClassification::Success => {
                entry.last_success = Some(now);
                entry.consecutive_transient_failures = 0;
                entry.reason = HealthReason::Healthy;
                entry.cooldown_until = None;
            }
            TerminalClassification::TokenExpired => {
                entry.reason = HealthReason::ReauthRequired;
                entry.cooldown_until = None;
            }
            TerminalClassification::TokenExhausted => {
                entry.reason = HealthReason::PlanExhausted;
                entry.cooldown_until = Some(now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_EXHAUSTED_COOLDOWN_SECS",
                    DEFAULT_EXHAUSTED_COOLDOWN_SECS,
                )));
            }
            TerminalClassification::Recoverable => {
                entry.reason = HealthReason::TransientFailure;
                entry.consecutive_transient_failures =
                    entry.consecutive_transient_failures.saturating_add(1);
                entry.cooldown_until = Some(now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_RECOVERABLE_BACKOFF_SECS",
                    DEFAULT_RECOVERABLE_BACKOFF_SECS,
                )));
            }
            TerminalClassification::SessionLimit => {
                entry.reason = HealthReason::SessionLimit;
                entry.cooldown_until = Some(now.saturating_add(cooldown_from_env(
                    "LOOM_CODEX_SESSION_BACKOFF_SECS",
                    DEFAULT_SESSION_BACKOFF_SECS,
                )));
            }
            TerminalClassification::Timeout
            | TerminalClassification::Fatal
            | TerminalClassification::CwdDeleted
            | TerminalClassification::ModelRefusal => unreachable!(),
        }
        state.accounts.push(entry);
        state
            .accounts
            .sort_by(|a, b| (a.provider as u8, &a.name).cmp(&(b.provider as u8, &b.name)));
        Ok(())
    })
}

/// Clear an auth hold only after the caller has independently verified reauth.
pub fn clear_reauth(workspace: &Path, id: &AccountId, provenance: &str) -> Result<()> {
    with_state(workspace, |state| {
        let entry = state
            .accounts
            .iter_mut()
            .find(|entry| entry.id() == *id)
            .ok_or_else(|| anyhow!("no health record for {:?}/{}", id.provider, id.name))?;
        if entry.reason != HealthReason::ReauthRequired {
            bail!("account is not awaiting reauthentication");
        }
        entry.reason = HealthReason::Healthy;
        entry.cooldown_until = None;
        entry.consecutive_transient_failures = 0;
        entry.updated_at = now_epoch();
        entry.signal_provenance = provenance.to_string();
        Ok(())
    })
}

pub fn account_health(workspace: &Path, id: &AccountId) -> Result<Option<AccountHealth>> {
    Ok(read_state(workspace)?
        .accounts
        .into_iter()
        .find(|entry| entry.id() == *id))
}

pub fn select_healthy_at(
    workspace: &Path,
    provider: AccountProvider,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Result<AccountDescriptor> {
    with_state(workspace, |state| {
        for entry in &mut state.accounts {
            if entry.reason == HealthReason::TransientFailure
                && entry.cooldown_until.is_some_and(|deadline| deadline <= now)
            {
                entry.reason = HealthReason::Healthy;
                entry.cooldown_until = None;
                entry.consecutive_transient_failures = 0;
                entry.updated_at = now;
            }
        }
        let health: HashMap<AccountId, AccountHealth> = state
            .accounts
            .iter()
            .cloned()
            .map(|entry| (entry.id(), entry))
            .collect();
        let mut candidates: Vec<_> = inventory
            .iter()
            .filter(|account| account.id.provider == provider && account.enabled)
            .filter(|account| {
                health
                    .get(&account.id)
                    .is_none_or(|entry| entry.is_eligible_at(now))
            })
            .cloned()
            .collect();
        candidates.sort_by_key(|account| {
            (
                health
                    .get(&account.id)
                    .map_or(0, |entry| entry.consecutive_transient_failures),
                account.id.name.clone(),
            )
        });
        if candidates.is_empty() {
            let reasons = inventory
                .iter()
                .filter(|account| account.id.provider == provider)
                .map(|account| {
                    let reason = if !account.enabled {
                        "disabled".to_string()
                    } else if let Some(entry) = health.get(&account.id) {
                        match entry.reason {
                            HealthReason::ReauthRequired => "reauth_required".to_string(),
                            _ => entry.cooldown_until.map_or_else(
                                || "unavailable".to_string(),
                                |until| format!("cooldown_until={until}"),
                            ),
                        }
                    } else {
                        "unavailable".to_string()
                    };
                    format!("{}={reason}", account.id.name)
                })
                .collect();
            return Err(NoHealthyAccountError { provider, reasons }.into());
        }
        let fewest = health
            .get(&candidates[0].id)
            .map_or(0, |entry| entry.consecutive_transient_failures);
        candidates.retain(|account| {
            health
                .get(&account.id)
                .map_or(0, |entry| entry.consecutive_transient_failures)
                == fewest
        });
        let cursor_key = format!("{provider:?}").to_ascii_lowercase();
        let cursor = state.cursors.entry(cursor_key).or_default();
        let chosen = candidates[*cursor as usize % candidates.len()].clone();
        *cursor = cursor.saturating_add(1);
        Ok(chosen)
    })
}

pub fn provider_capacity_at(
    workspace: &Path,
    provider: AccountProvider,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Result<ProviderCapacity> {
    let state = read_state(workspace)?;
    let health: HashMap<_, _> = state
        .accounts
        .into_iter()
        .map(|entry| (entry.id(), entry))
        .collect();
    let accounts: Vec<_> = inventory
        .iter()
        .filter(|account| account.id.provider == provider)
        .collect();
    let enabled: Vec<_> = accounts.iter().filter(|account| account.enabled).collect();
    let reauth_required = enabled
        .iter()
        .filter(|account| {
            health
                .get(&account.id)
                .is_some_and(|entry| entry.reason == HealthReason::ReauthRequired)
        })
        .count();
    let cooldown = enabled
        .iter()
        .filter(|account| {
            health.get(&account.id).is_some_and(|entry| {
                entry.reason != HealthReason::ReauthRequired
                    && entry.cooldown_until.is_some_and(|until| until > now)
            })
        })
        .count();
    Ok(ProviderCapacity {
        provider,
        raw: accounts.len(),
        enabled: enabled.len(),
        healthy: enabled.len().saturating_sub(reauth_required + cooldown),
        cooldown,
        reauth_required,
        observed_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens_pool::account_registry::{CredentialKind, InventoryProvenance};

    fn descriptor(provider: AccountProvider, name: &str) -> AccountDescriptor {
        AccountDescriptor {
            id: AccountId {
                provider,
                name: name.into(),
            },
            credential_kind: CredentialKind::CodexHome,
            credential_reference: PathBuf::from(name),
            enabled: true,
            provenance: InventoryProvenance::Shared,
        }
    }

    #[test]
    fn exhausted_fails_over_and_expires_at_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = vec![
            descriptor(AccountProvider::Codex, "a"),
            descriptor(AccountProvider::Codex, "b"),
        ];
        record_terminal_at(
            tmp.path(),
            &accounts[0].id,
            TerminalClassification::TokenExhausted,
            "adapter_v1",
            100,
        )
        .unwrap();
        assert_eq!(
            select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
                .unwrap()
                .id
                .name,
            "b"
        );
        assert!(select_healthy_at(
            tmp.path(),
            AccountProvider::Codex,
            &accounts,
            100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
        )
        .is_ok());
    }

    #[test]
    fn expired_survives_time_and_success_until_explicit_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let account = descriptor(AccountProvider::Codex, "a");
        record_terminal_at(
            tmp.path(),
            &account.id,
            TerminalClassification::TokenExpired,
            "adapter_v1",
            1,
        )
        .unwrap();
        record_terminal_at(
            tmp.path(),
            &account.id,
            TerminalClassification::Success,
            "adapter_v1",
            u64::MAX - 1,
        )
        .unwrap();
        assert!(select_healthy_at(
            tmp.path(),
            AccountProvider::Codex,
            std::slice::from_ref(&account),
            u64::MAX
        )
        .is_err());
        clear_reauth(tmp.path(), &account.id, "verified_reauth").unwrap();
        assert!(select_healthy_at(tmp.path(), AccountProvider::Codex, &[account], u64::MAX).is_ok());
    }

    #[test]
    fn expired_survives_all_later_runtime_feedback_until_explicit_clear() {
        for classification in [
            TerminalClassification::TokenExhausted,
            TerminalClassification::Recoverable,
            TerminalClassification::SessionLimit,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let account = descriptor(AccountProvider::Codex, "a");
            record_terminal_at(
                tmp.path(),
                &account.id,
                TerminalClassification::TokenExpired,
                "adapter_v1",
                1,
            )
            .unwrap();
            record_terminal_at(tmp.path(), &account.id, classification, "adapter_v1", 2).unwrap();

            let health = account_health(tmp.path(), &account.id).unwrap().unwrap();
            assert_eq!(health.reason, HealthReason::ReauthRequired);
            assert_eq!(health.cooldown_until, None);
            assert!(select_healthy_at(
                tmp.path(),
                AccountProvider::Codex,
                std::slice::from_ref(&account),
                u64::MAX
            )
            .is_err());
        }
    }

    #[test]
    fn expired_transient_backoff_restores_fair_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = vec![
            descriptor(AccountProvider::Codex, "a"),
            descriptor(AccountProvider::Codex, "b"),
        ];
        record_terminal_at(
            tmp.path(),
            &accounts[0].id,
            TerminalClassification::Recoverable,
            "adapter_v1",
            100,
        )
        .unwrap();

        assert_eq!(
            select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
                .unwrap()
                .id
                .name,
            "b"
        );

        let deadline = 100 + DEFAULT_RECOVERABLE_BACKOFF_SECS;
        let first =
            select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, deadline).unwrap();
        let second =
            select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, deadline).unwrap();
        assert_ne!(first.id, second.id);
        assert!([first.id.name, second.id.name].contains(&"a".to_string()));

        let recovered = account_health(tmp.path(), &accounts[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.reason, HealthReason::Healthy);
        assert_eq!(recovered.cooldown_until, None);
        assert_eq!(recovered.consecutive_transient_failures, 0);
    }

    #[test]
    fn neutral_failures_do_not_poison_and_same_names_are_provider_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let codex = descriptor(AccountProvider::Codex, "same");
        let claude = descriptor(AccountProvider::Claude, "same");
        record_terminal_at(
            tmp.path(),
            &codex.id,
            TerminalClassification::TokenExpired,
            "adapter_v1",
            1,
        )
        .unwrap();
        for category in [
            TerminalClassification::Timeout,
            TerminalClassification::Fatal,
            TerminalClassification::CwdDeleted,
            TerminalClassification::ModelRefusal,
        ] {
            record_terminal_at(tmp.path(), &claude.id, category, "adapter_v1", 2).unwrap();
        }
        assert!(account_health(tmp.path(), &claude.id).unwrap().is_none());
        assert_eq!(
            account_health(tmp.path(), &codex.id)
                .unwrap()
                .unwrap()
                .reason,
            HealthReason::ReauthRequired
        );
    }

    #[test]
    fn round_robin_is_persistent_and_capacity_is_honest() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = vec![
            descriptor(AccountProvider::Codex, "a"),
            descriptor(AccountProvider::Codex, "b"),
        ];
        let first = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
        let second = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
        assert_ne!(first.id, second.id);
        let capacity =
            provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
        assert_eq!((capacity.raw, capacity.enabled, capacity.healthy), (2, 2, 2));
    }

    #[test]
    fn state_never_contains_credentials_or_raw_output() {
        let tmp = tempfile::tempdir().unwrap();
        let id = AccountId {
            provider: AccountProvider::Codex,
            name: "safe-name".into(),
        };
        record_terminal_at(tmp.path(), &id, TerminalClassification::Recoverable, "adapter_v1", 1)
            .unwrap();
        let state = fs::read_to_string(state_path(tmp.path())).unwrap();
        assert!(!state.contains("auth.json"));
        assert!(!state.contains("recognizable-secret"));
    }

    #[test]
    fn malformed_and_unknown_schema_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".loom")).unwrap();
        fs::write(state_path(tmp.path()), r#"{"version":99,"accounts":[]}"#).unwrap();
        assert!(read_state(tmp.path()).is_err());
        fs::write(state_path(tmp.path()), "{broken").unwrap();
        assert!(read_state(tmp.path()).is_err());
    }
}
