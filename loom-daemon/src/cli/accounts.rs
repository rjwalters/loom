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
                 auth-permissions={}, owner={}, session={}, login={:?}",
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
                // Which probe transport produced `login` below: the account's
                // own session container (issue #6927) or a host-direct
                // `codex login status`.
                if status.session_managed {
                    "container"
                } else {
                    "host"
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
            email,
            json,
        } => {
            require_codex(&provider)?;
            print_status(
                &preserve_login_exit(service.add_with_email(&name, device_auth, email.as_deref()))?,
                json,
            )
        }
        AccountsAction::Import {
            provider,
            name,
            auth_file,
            email,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.import_with_email(&name, &auth_file, email.as_deref())?, json)
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
        AccountsAction::Check {
            provider,
            ranking,
            json,
        } => {
            require_codex(&provider)?;
            run_availability_check(&workspace, ranking, json)
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
        AccountsAction::Rename {
            provider,
            old_name,
            new_name,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.rename(&old_name, &new_name)?, json)
        }
        AccountsAction::Adopt {
            provider,
            name,
            json,
        } => {
            require_codex(&provider)?;
            print_status(&service.adopt(&name)?, json)
        }
    }
}

/// `loom-daemon accounts check [--ranking]` (issue #8407) — the Codex
/// availability probe, the `tokens check --ranking` analogue.
///
/// # Exit-code contract
///
/// | Exit | Meaning |
/// |---|---|
/// | `0` | A report was produced. **Includes a host with no Codex profiles**, which says so and exits `0` — "no pool" is not a pool failure, and a script gating on this must not treat an un-provisioned host as an outage. |
/// | `1` | Codex accounts exist but **none is dispatchable right now** — every one is rate-limited, exhausted, blocked, disabled, or errored. This is the pre-dispatch signal that routing codex work here will fail at selection. |
/// | other | The ordinary CLI error path (unreadable registry, unwritable ranking file). |
///
/// This is deliberately stricter than `tokens check`, which exits `1` only
/// when every row is `error`/`skipped`: that command's consumers re-derive
/// selectability from `.ranking` themselves, whereas this one exists
/// precisely to answer "can this host take codex work right now".
///
/// Output is secret-free by construction: every field comes from the account
/// registry, `account-health.json`, or the numeric `rate_limits` snapshot —
/// `auth.json` is never opened.
fn run_availability_check(workspace: &std::path::Path, ranking: bool, json: bool) -> Result<()> {
    use loom_daemon::tokens_pool::codex_check::{self, CheckOptions};

    eprintln!("Resolved workspace: {}", workspace.display());
    let (report, effects) = codex_check::run_check(
        workspace,
        CheckOptions {
            write_ranking: ranking,
        },
        chrono::Utc::now(),
    )?;

    if report.accounts.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace": workspace.display().to_string(),
                    "accounts": [],
                    "ranking_path": serde_json::Value::Null,
                }))?
            );
        } else {
            println!(
                "No Codex accounts registered on this host; nothing to probe. Add one with \
                 `loom-daemon accounts add codex <name>`."
            );
        }
        return Ok(());
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "workspace": workspace.display().to_string(),
                "report": report.to_json(),
                "ranking_path": effects
                    .ranking_written
                    .as_ref()
                    .map(|path| path.display().to_string()),
                "marked_exhausted": effects.marked_exhausted,
                "cleared": effects.cleared,
            }))?
        );
    } else {
        println!("{}", codex_check::format_table(&report));
        if let Some(path) = &effects.ranking_written {
            println!("Ranking written to {}", path.display());
        }
        for name in &effects.marked_exhausted {
            println!("Held codex/{name} from selection until its window rolls over.");
        }
        for name in &effects.cleared {
            println!("Released codex/{name}'s exhaustion hold — measured headroom is newer.");
        }
    }

    if !codex_check::has_usable_account(&report) {
        std::process::exit(1);
    }
    Ok(())
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
                 mount={}, session_managed={}, workspace={})",
                status.name,
                if status.running { "running" } else { "stopped" },
                status.container_name,
                status.container_id.as_deref().unwrap_or("-"),
                status.image.as_deref().unwrap_or("-"),
                status.started_at.as_deref().unwrap_or("-"),
                status.codex_home.display(),
                status.mount_path,
                status.session_managed,
                status
                    .workspace
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |w| w.display().to_string()),
            );
        }
        Ok(())
    }

    match action {
        SessionAction::Start {
            name,
            image,
            workspace: workspace_arg,
            json,
        } => {
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, image);
            print_session_status(
                &lifecycle.start_with_workspace(&name, workspace_arg.as_deref())?,
                json,
            )
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
        SessionAction::Shell {
            name,
            workspace: workspace_arg,
            args,
        } => {
            let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
            let code = lifecycle.shell(&name, workspace_arg.as_deref(), &args)?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}
