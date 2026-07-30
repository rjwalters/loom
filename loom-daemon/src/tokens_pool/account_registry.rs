//! Provider-aware account identity and raw inventory.
//!
//! Claude remains backed by the byte-compatible token pool. Codex accounts are
//! logical names bound to directories below a machine-level profile root; this
//! module never reads `auth.json`.

use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::paths::{codex_profile_root, per_repo_accounts_file, resolve_tokens_dir};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountProvider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId {
    pub provider: AccountProvider,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    OauthTokenFile,
    CodexHome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryProvenance {
    Repo,
    Shared,
}

/// Public metadata is deliberately non-secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDescriptor {
    pub id: AccountId,
    pub credential_kind: CredentialKind,
    pub credential_reference: PathBuf,
    pub enabled: bool,
    pub provenance: InventoryProvenance,
}

/// Launch-time credential binding. It is intentionally not serializable, and
/// its Debug representation never prints a Claude token.
#[derive(Clone, PartialEq, Eq)]
pub enum AccountBinding {
    ClaudeOauthToken { token_file: PathBuf, token: String },
    CodexHome { directory: PathBuf },
}

impl fmt::Debug for AccountBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClaudeOauthToken { token_file, .. } => f
                .debug_struct("ClaudeOauthToken")
                .field("token_file", token_file)
                .field("token", &"[REDACTED]")
                .finish(),
            Self::CodexHome { directory } => f
                .debug_struct("CodexHome")
                .field("directory", directory)
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SelectedAccount {
    pub id: AccountId,
    pub binding: AccountBinding,
    pub mode: &'static str,
}

impl fmt::Debug for SelectedAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectedAccount")
            .field("id", &self.id)
            .field("binding", &self.binding)
            .field("mode", &self.mode)
            .finish()
    }
}

impl SelectedAccount {
    /// Non-secret observability environment. Claude keeps its historical name.
    #[must_use]
    pub fn identity_env(&self) -> Vec<(&'static str, String)> {
        let provider = match self.id.provider {
            AccountProvider::Claude => "claude",
            AccountProvider::Codex => "codex",
        };
        let mut env = vec![
            ("LOOM_ACCOUNT_PROVIDER", provider.to_string()),
            ("LOOM_ACCOUNT_NAME", self.id.name.clone()),
        ];
        if self.id.provider == AccountProvider::Claude {
            env.push(("LOOM_TOKEN_NAME", self.id.name.clone()));
        }
        env
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(default = "registry_version")]
    version: u32,
    accounts: Vec<RegistryEntry>,
}

const fn registry_version() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryEntry {
    provider: AccountProvider,
    name: String,
    credential_kind: CredentialKind,
    credential_reference: String,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

fn validate_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    if name.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
        || name.contains(['/', '\\'])
    {
        bail!("invalid account name {name:?}: expected one relative path component");
    }
    Ok(())
}

fn validate_codex_directory(root: &Path, reference: &str) -> Result<PathBuf> {
    validate_name(reference)?;
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("Codex profile root {} is unavailable", root.display()))?;
    let candidate = root.join(reference);
    let metadata = std::fs::metadata(&candidate)
        .with_context(|| format!("Codex profile {reference:?} does not exist"))?;
    if !metadata.is_dir() {
        bail!("Codex profile {reference:?} is not a directory");
    }
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("Codex profile {reference:?} cannot be resolved"))?;
    if !canonical.starts_with(&canonical_root) || canonical == canonical_root {
        bail!("Codex profile {reference:?} escapes the configured profile root");
    }
    Ok(canonical)
}

fn read_repo_registry(workspace: &Path) -> Result<Option<Vec<RegistryEntry>>> {
    let path = per_repo_accounts_file(workspace);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read account registry {}", path.display()))?;
    let registry: RegistryFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid account registry {}", path.display()))?;
    if registry.version != 1 {
        bail!("unsupported account registry version {}", registry.version);
    }
    let mut ids = HashSet::new();
    for entry in &registry.accounts {
        validate_name(&entry.name)?;
        if !ids.insert((entry.provider, entry.name.clone())) {
            bail!("duplicate account identity {:?}/{}", entry.provider, entry.name);
        }
    }
    Ok(Some(registry.accounts))
}

fn claude_inventory(workspace: &Path) -> Result<Vec<AccountDescriptor>> {
    let pool = resolve_tokens_dir(workspace);
    let provenance = if pool == super::paths::per_repo_tokens_dir(workspace) {
        InventoryProvenance::Repo
    } else {
        InventoryProvenance::Shared
    };
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&pool) else {
        return Ok(out);
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            || path.extension().and_then(|v| v.to_str()) != Some("token")
        {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow!("Claude token filename is not valid UTF-8"))?
            .to_string();
        out.push(AccountDescriptor {
            id: AccountId {
                provider: AccountProvider::Claude,
                name,
            },
            credential_kind: CredentialKind::OauthTokenFile,
            credential_reference: path,
            enabled: true,
            provenance,
        });
    }
    out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    Ok(out)
}

fn codex_inventory(workspace: &Path) -> Result<Vec<AccountDescriptor>> {
    let root = codex_profile_root()
        .ok_or_else(|| anyhow!("Codex profile root is disabled by LOOM_CODEX_PROFILE_ROOT"))?;
    if let (Ok(root), Ok(workspace)) = (root.canonicalize(), workspace.canonicalize()) {
        if root.starts_with(&workspace) {
            bail!("Codex profile root {} must not be repository-local", root.display());
        }
    }
    let entries = read_repo_registry(workspace)?;
    let mut out = Vec::new();
    if let Some(entries) = entries {
        for entry in entries
            .into_iter()
            .filter(|entry| entry.provider == AccountProvider::Codex)
        {
            if entry.credential_kind != CredentialKind::CodexHome {
                bail!("Codex account {:?} must use codex_home", entry.name);
            }
            let directory = validate_codex_directory(&root, &entry.credential_reference)?;
            out.push(AccountDescriptor {
                id: AccountId {
                    provider: AccountProvider::Codex,
                    name: entry.name,
                },
                credential_kind: CredentialKind::CodexHome,
                credential_reference: directory,
                enabled: entry.enabled,
                provenance: InventoryProvenance::Repo,
            });
        }
    } else {
        let canonical_root = match root.canonicalize() {
            Ok(path) => path,
            Err(_) => return Ok(out),
        };
        for entry in std::fs::read_dir(&root)?.filter_map(std::result::Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if validate_name(&name).is_err() {
                continue;
            }
            let Ok(directory) = validate_codex_directory(&canonical_root, &name) else {
                continue;
            };
            out.push(AccountDescriptor {
                id: AccountId {
                    provider: AccountProvider::Codex,
                    name,
                },
                credential_kind: CredentialKind::CodexHome,
                credential_reference: directory,
                enabled: true,
                provenance: InventoryProvenance::Shared,
            });
        }
    }
    out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    Ok(out)
}

pub fn account_inventory(
    workspace: &Path,
    provider: AccountProvider,
) -> Result<Vec<AccountDescriptor>> {
    match provider {
        AccountProvider::Claude => claude_inventory(workspace),
        AccountProvider::Codex => codex_inventory(workspace),
    }
}

/// Raw enabled candidates only; health and cooldown policy belongs to #4493.
pub fn account_capacity(workspace: &Path, provider: AccountProvider) -> Result<usize> {
    Ok(account_inventory(workspace, provider)?
        .into_iter()
        .filter(|account| account.enabled)
        .count())
}

pub fn select_account(workspace: &Path, provider: AccountProvider) -> Result<SelectedAccount> {
    match provider {
        AccountProvider::Claude => {
            let selected = super::select::select_token(workspace, None)?;
            Ok(SelectedAccount {
                id: AccountId {
                    provider,
                    name: selected.name,
                },
                binding: AccountBinding::ClaudeOauthToken {
                    token_file: selected.file,
                    token: selected.key,
                },
                mode: selected.mode,
            })
        }
        AccountProvider::Codex => {
            let descriptor = account_inventory(workspace, provider)?
                .into_iter()
                .find(|account| account.enabled)
                .ok_or_else(|| anyhow!("no enabled Codex profiles are available"))?;
            Ok(SelectedAccount {
                id: descriptor.id,
                binding: AccountBinding::CodexHome {
                    directory: descriptor.credential_reference,
                },
                mode: "inventory",
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::collections::HashSet;
    use std::fs;

    fn registry(workspace: &Path, body: &str) {
        let path = per_repo_accounts_file(workspace);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn provider_is_part_of_identity_and_serialization() {
        let claude = AccountId {
            provider: AccountProvider::Claude,
            name: "alice".into(),
        };
        let codex = AccountId {
            provider: AccountProvider::Codex,
            name: "alice".into(),
        };
        assert_ne!(claude, codex);
        assert_eq!(HashSet::from([claude.clone(), codex.clone()]).len(), 2);
        assert_ne!(serde_json::to_string(&claude).unwrap(), serde_json::to_string(&codex).unwrap());
    }

    #[test]
    fn binding_debug_redacts_claude_secret() {
        let binding = AccountBinding::ClaudeOauthToken {
            token_file: "/tmp/alice.token".into(),
            token: "recognizable-super-secret".into(),
        };
        let debug = format!("{binding:?}");
        assert!(!debug.contains("recognizable-super-secret"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    #[serial]
    fn codex_repo_registry_restricts_profiles_and_never_reads_auth() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        fs::create_dir(profiles.path().join("alice")).unwrap();
        fs::create_dir(profiles.path().join("bob")).unwrap();
        fs::write(profiles.path().join("alice/auth.json"), "recognizable-secret").unwrap();
        registry(
            workspace.path(),
            r#"{"version":1,"accounts":[{"provider":"codex","name":"work","credential_kind":"codex_home","credential_reference":"alice","enabled":true}]}"#,
        );
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        let accounts = account_inventory(workspace.path(), AccountProvider::Codex).unwrap();
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id.name, "work");
        assert!(!format!("{accounts:?}").contains("recognizable-secret"));
        assert!(!serde_json::to_string(&accounts)
            .unwrap()
            .contains("recognizable-secret"));
    }

    #[test]
    #[serial]
    fn codex_falls_back_to_shared_directories_and_counts_by_provider() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        fs::create_dir(profiles.path().join("alice")).unwrap();
        fs::create_dir(profiles.path().join("bob")).unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        assert_eq!(account_capacity(workspace.path(), AccountProvider::Codex).unwrap(), 2);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn rejects_traversal_absolute_non_directory_and_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(profiles.path().join("file"), "not-dir").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), profiles.path().join("escape")).unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        for reference in ["../outside", "/absolute", "a/b", "file"] {
            registry(
                workspace.path(),
                &format!(
                    r#"{{"version":1,"accounts":[{{"provider":"codex","name":"x","credential_kind":"codex_home","credential_reference":"{reference}","enabled":true}}]}}"#
                ),
            );
            assert!(account_inventory(workspace.path(), AccountProvider::Codex).is_err());
        }
        #[cfg(unix)]
        {
            registry(
                workspace.path(),
                r#"{"version":1,"accounts":[{"provider":"codex","name":"x","credential_kind":"codex_home","credential_reference":"escape","enabled":true}]}"#,
            );
            assert!(account_inventory(workspace.path(), AccountProvider::Codex).is_err());
        }
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn rejects_duplicates_and_wrong_kind() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        fs::create_dir(profiles.path().join("alice")).unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        registry(
            workspace.path(),
            r#"{"accounts":[{"provider":"codex","name":"alice","credential_kind":"codex_home","credential_reference":"alice"},{"provider":"codex","name":"alice","credential_kind":"codex_home","credential_reference":"alice"}]}"#,
        );
        assert!(account_inventory(workspace.path(), AccountProvider::Codex)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
        registry(
            workspace.path(),
            r#"{"accounts":[{"provider":"codex","name":"alice","credential_kind":"oauth_token_file","credential_reference":"alice"}]}"#,
        );
        assert!(account_inventory(workspace.path(), AccountProvider::Codex).is_err());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn identity_env_is_provider_safe() {
        let selected = SelectedAccount {
            id: AccountId {
                provider: AccountProvider::Codex,
                name: "alice".into(),
            },
            binding: AccountBinding::CodexHome {
                directory: "/tmp/alice".into(),
            },
            mode: "inventory",
        };
        let env = selected.identity_env();
        assert!(env.contains(&("LOOM_ACCOUNT_PROVIDER", "codex".into())));
        assert!(env.contains(&("LOOM_ACCOUNT_NAME", "alice".into())));
        assert!(!env.iter().any(|(key, _)| *key == "LOOM_TOKEN_NAME"));
    }
}
