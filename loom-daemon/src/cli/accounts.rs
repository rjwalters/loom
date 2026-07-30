//! `loom-daemon accounts` handler (Issue #4492): secret-safe machine-level
//! Codex account lifecycle commands over
//! `loom_daemon::tokens_pool::account_lifecycle`.

use anyhow::{anyhow, Result};

use super::tokens::resolve_tokens_workspace;
use crate::AccountsAction;

pub(crate) fn handle_accounts_command(action: AccountsAction, workspace: &str) -> Result<()> {
    use loom_daemon::tokens_pool::account_lifecycle::{
        AccountLifecycle, AccountStatus, ProcessCodexRunner,
    };

    fn require_codex(provider: &str) -> Result<()> {
        if provider.eq_ignore_ascii_case("codex") {
            Ok(())
        } else {
            Err(anyhow!(
                "provider {provider:?} is not supported by `accounts`; Claude token behavior is unchanged"
            ))
        }
    }

    fn print_status(status: &AccountStatus, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(status)?);
        } else {
            println!(
                "codex/{}: {} ({:?}); credential={}, directory-permissions={}, \
                 auth-permissions={}, owner={}, login={:?}",
                status.name,
                if status.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                status.provenance,
                status.diagnostics.auth_shape,
                if status.diagnostics.directory_mode_valid {
                    "valid"
                } else {
                    "unsafe"
                },
                if status.diagnostics.auth_mode_valid {
                    "valid"
                } else {
                    "unsafe"
                },
                if status.diagnostics.owner_valid {
                    "valid"
                } else {
                    "mismatch"
                },
                status.login_state,
            );
        }
        Ok(())
    }

    let workspace = resolve_tokens_workspace(workspace)?;
    let service = AccountLifecycle::new(workspace, ProcessCodexRunner)?;
    match action {
        AccountsAction::Add {
            provider,
            name,
            device_auth,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.add(&name, device_auth)?, json)
        }
        AccountsAction::Import {
            provider,
            name,
            auth_file,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.import(&name, &auth_file)?, json)
        }
        AccountsAction::List { provider, json } => {
            require_codex(&provider)?;
            let statuses = service.list(false)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&statuses)?);
            } else if statuses.is_empty() {
                println!("No Codex accounts registered.");
            } else {
                for status in &statuses {
                    print_status(status, false)?;
                }
            }
            Ok(())
        }
        AccountsAction::Status {
            provider,
            name,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.status(&name)?, json)
        }
        AccountsAction::Disable {
            provider,
            name,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.disable(&name)?, json)
        }
        AccountsAction::Enable {
            provider,
            name,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.enable(&name)?, json)
        }
        AccountsAction::Reauth {
            provider,
            name,
            device_auth,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.reauth(&name, device_auth)?, json)
        }
        AccountsAction::Remove {
            provider,
            name,
            purge,
            json,
        } => {
            require_codex(&provider)?;
            let result = service.remove(&name, purge)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(reference) = result.recovery_reference {
                println!(
                    "Retired codex/{} to private quarantine as {reference}. Recovery remains \
                     machine-local; re-import its auth.json to restore.",
                    result.name
                );
            } else {
                println!("Irreversibly purged codex/{}.", result.name);
            }
            Ok(())
        }
    }
}
