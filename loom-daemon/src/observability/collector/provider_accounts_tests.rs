use super::*;
use crate::tokens_pool::health::{record_terminal_at, DEFAULT_EXHAUSTED_COOLDOWN_SECS};
use crate::tokens_pool::{select_healthy_at, AccountId, TerminalClassification};
use serial_test::serial;

struct RegistryFixture {
    workspace: tempfile::TempDir,
    _profiles: tempfile::TempDir,
    previous_profile_root: Option<std::ffi::OsString>,
}

impl RegistryFixture {
    fn new(accounts: &[(&str, bool)]) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".loom")).unwrap();
        let accounts: Vec<_> = accounts
            .iter()
            .map(|(name, enabled)| {
                std::fs::create_dir(profiles.path().join(name)).unwrap();
                serde_json::json!({
                    "provider": "codex", "name": name,
                    "credential_kind": "codex_home", "credential_reference": name,
                    "enabled": enabled,
                })
            })
            .collect();
        std::fs::write(
            workspace.path().join(".loom/accounts.json"),
            serde_json::to_vec(&serde_json::json!({"version": 1, "accounts": accounts})).unwrap(),
        )
        .unwrap();
        let previous_profile_root =
            std::env::var_os(crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV);
        std::env::set_var(crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV, profiles.path());
        Self {
            workspace,
            _profiles: profiles,
            previous_profile_root,
        }
    }
}

impl Drop for RegistryFixture {
    fn drop(&mut self) {
        let key = crate::tokens_pool::paths::CODEX_PROFILE_ROOT_ENV;
        match &self.previous_profile_root {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

#[test]
#[serial(codex_profile_root)]
fn registry_provider_accounts_omit_unreadable_health() {
    let fixture = RegistryFixture::new(&[("work", true)]);
    let workspace = fixture.workspace.path();
    let path = workspace.join(".loom/account-health.json");
    let inventory = account_inventory(workspace, AccountProvider::Codex).unwrap();
    for contents in ["not-json", r#"{"version":999,"accounts":[]}"#] {
        std::fs::write(&path, contents).unwrap();
        assert!(select_healthy_at(workspace, AccountProvider::Codex, &inventory, 1).is_err());
        assert!(sample_registry_provider_accounts(workspace).is_empty());
    }
    // A directory produces a real read failure even when tests run as root.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(select_healthy_at(workspace, AccountProvider::Codex, &inventory, 1).is_err());
    assert!(sample_registry_provider_accounts(workspace).is_empty());
}

#[test]
#[serial(codex_profile_root)]
fn registry_provider_accounts_preserve_missing_health_and_report_holds() {
    let fixture = RegistryFixture::new(&[
        ("missing", true),
        ("healthy", true),
        ("cooldown", true),
        ("reauth", true),
        ("expired", true),
        ("disabled", false),
    ]);
    let workspace = fixture.workspace.path();
    // A missing state file is a successful empty snapshot, not a read error.
    let initial = sample_registry_provider_accounts(workspace);
    assert_eq!(initial.len(), 5);
    assert!(initial.iter().all(|account| !account.exhausted));
    let now = u64::try_from(Utc::now().timestamp()).unwrap();
    for (name, result, at) in [
        ("healthy", TerminalClassification::Success, now),
        ("cooldown", TerminalClassification::TokenExhausted, now),
        ("reauth", TerminalClassification::TokenExpired, now),
        (
            "expired",
            TerminalClassification::TokenExhausted,
            now - DEFAULT_EXHAUSTED_COOLDOWN_SECS - 1,
        ),
        ("disabled", TerminalClassification::Success, now),
    ] {
        record_terminal_at(
            workspace,
            &AccountId {
                provider: AccountProvider::Codex,
                name: name.into(),
            },
            result,
            "collector_test",
            at,
        )
        .unwrap();
    }
    let accounts = sample_registry_provider_accounts(workspace);
    assert_eq!(accounts.len(), 5);
    assert!(!accounts.iter().any(|account| account.account == "disabled"));
    for account in accounts {
        assert_eq!(account.provider, "codex");
        assert_eq!(account.rank, None);
        assert_eq!(account.usage_fraction, None);
        assert_eq!(account.exhausted, matches!(account.account.as_str(), "cooldown" | "reauth"));
        let deadline = if account.account == "cooldown" {
            DateTime::<Utc>::from_timestamp(
                i64::try_from(now + DEFAULT_EXHAUSTED_COOLDOWN_SECS).unwrap(),
                0,
            )
        } else {
            None
        };
        assert_eq!(account.limit_window_reset_at, deadline);
    }
}
