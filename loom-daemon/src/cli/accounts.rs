//! `loom-daemon accounts` handler (Issue #4492): secret-safe machine-level
//! Codex account lifecycle commands over
//! `loom_daemon::tokens_pool::account_lifecycle`.

use anyhow::{anyhow, Result};

use super::tokens::resolve_tokens_workspace;
use crate::{AccountsAction, SessionAction};

pub(crate) fn handle_accounts_command(action: AccountsAction, workspace: &str) -> Result<()> {
    use loom_daemon::tokens_pool::account_lifecycle::{
        login_exit_code, AccountLifecycle, AccountStatus, ProcessCodexRunner,
    };

    fn preserve_login_exit<T>(result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(code) = login_exit_code(&error) {
                    eprintln!("error: {error}");
                    std::process::exit(code);
                }
                Err(error)
            }
        }
    }

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
    let service = AccountLifecycle::new(workspace.clone(), ProcessCodexRunner)?;
    match action {
        AccountsAction::Add {
            provider,
            name,
            device_auth,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&preserve_login_exit(service.add(&name, device_auth))?, json)
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
            print_status(&preserve_login_exit(service.reauth(&name, device_auth))?, json)
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
        AccountsAction::Session { action } => handle_session_command(action, workspace),
    }
}

fn handle_session_command(action: SessionAction, workspace: std::path::PathBuf) -> Result<()> {
    use loom_daemon::tokens_pool::session_lifecycle::{
        ProcessContainerRunner, SessionLifecycle, SessionStatus,
    };

    fn print_session_status(status: &SessionStatus, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(status)?);
        } else {
            println!(
                "{}: {} (container={}, id={}, image={}, started_at={}, codex_home={}, \
                 mount={}, session_managed={})",
                status.name,
                if status.running { "running" } else { "stopped" },
                status.container_name,
                status.container_id.as_deref().unwrap_or("-"),
                status.image.as_deref().unwrap_or("-"),
                status.started_at.as_deref().unwrap_or("-"),
                status.codex_home.display(),
                status.mount_path,
                status.session_managed,
            );
        }
        Ok(())
    }

    match action {
        SessionAction::Start { name, image, json } => {
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, image);
            print_session_status(&lifecycle.start(&name)?, json)
        }
        SessionAction::Stop { name, force, json } => {
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            print_session_status(&lifecycle.stop(&name, force)?, json)
        }
        SessionAction::Status { name, json } => {
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            print_session_status(&lifecycle.status(&name)?, json)
        }
        SessionAction::Attach { name } => {
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            let code = lifecycle.attach(&name)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}
