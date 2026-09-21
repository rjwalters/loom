//! `loom-daemon api-keys` handler (issue #8401): secret-safe lifecycle
//! commands over [`loom_daemon::api_keys_pool`], mirroring the verb shape of
//! `loom-daemon accounts` (Codex) and `loom-daemon tokens` (Claude OAuth).
//!
//! **No verb accepts key material on the command line.** `add` reads the key
//! from `--key-file` or standard input, because argv is visible to every
//! process on the host and lands in shell history. `list` and `health` are
//! secret-free by construction — they render
//! [`loom_daemon::api_keys_pool::ApiKeyAccount`], which cannot hold a value.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::PathBuf;

use super::tokens::resolve_tokens_workspace;
use loom_daemon::api_keys_pool::{
    bad_marks, limits,
    paths::{
        default_env_name, is_conventional_env_name, per_repo_api_keys_dir, pool_roots,
        resolve_provider_root,
    },
    registry, select, ApiKeyAccount,
};

#[derive(clap::Subcommand)]
pub enum ApiKeysAction {
    /// Register an API-key account. The key is read from `--key-file` or
    /// stdin, never from the command line.
    Add {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        /// File holding the key (use `-` for stdin). Omit to read stdin.
        #[arg(long, value_name = "PATH")]
        key_file: Option<PathBuf>,
        /// Environment variable the key is exported as. Defaults to
        /// `<PROVIDER>_API_KEY`.
        #[arg(long, value_name = "ENV")]
        env_var: Option<String>,
        /// Register into the shared machine-level pool (`~/.loom/api-keys`, or
        /// `LOOM_SHARED_API_KEYS_DIR`) instead of this repo's. Precedence is
        /// per provider: a repo's own accounts for a provider take that
        /// provider over for the repo; every other provider still resolves to
        /// the shared pool.
        #[arg(long)]
        shared: bool,
        /// Replace an existing account of the same name.
        #[arg(long)]
        force: bool,
        /// Most spawns that may hold this account at once — the provider's own
        /// concurrent-request ceiling (#8424). Omit for unbounded. Change it
        /// later with `api-keys limit`.
        #[arg(long, value_name = "N")]
        max_concurrent: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// List registered accounts and secret-free structural diagnostics.
    List {
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Make an account ineligible for selection without removing its key.
    Disable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Restore an account's eligibility.
    Enable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete an account's credential file from this host.
    Remove {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Declare (or clear) an account's concurrency cap — the most spawns that
    /// may hold it at once (#8424). An account at its cap is skipped in
    /// favour of another eligible account, and becomes selectable again as
    /// soon as one of those spawns exits.
    Limit {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        /// The cap. Must be > 0 — use `disable` to take an account out of
        /// selection entirely.
        #[arg(long, value_name = "N", conflicts_with = "unlimited")]
        max_concurrent: Option<u32>,
        /// Clear the cap (unbounded again).
        #[arg(long)]
        unlimited: bool,
        #[arg(long)]
        json: bool,
    },
    /// Per-provider pool health: totals, selectable count, and why each
    /// excluded account is excluded.
    Health {
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record an exhaustion/rate-limit mark against an account (issue #8401)
    /// — normally produced by classifying a harness's own error output
    /// (`loom_daemon::api_keys_pool::classify`), but may be invoked directly
    /// to simulate one. A bad-marked account drops out of selection until
    /// its reset horizon, the same way a disabled one does.
    MarkBad {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        /// Free-text explanation recorded with the mark and shown by `list`
        /// and `health`. Defaults to a generic operator attribution rather
        /// than an empty string, which rendered as a bare `bad-marked — ""`.
        #[arg(long, default_value = "marked bad by operator")]
        reason: String,
        /// Seconds until the account is selectable again. Omit for an
        /// indefinite mark (clear explicitly with `unblock`).
        #[arg(long)]
        cooldown_secs: Option<u64>,
        /// Scope the mark to one model class — the model id whose allowance
        /// ran out, e.g. `glm-5.3-flash` (#8424). The same account stays
        /// selectable for every other class. Omit to mark the whole account.
        #[arg(long, value_name = "CLASS")]
        model_class: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Clear an exhaustion/rate-limit mark before its reset horizon.
    Unblock {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        /// Clear only this model class's mark, leaving any account-wide mark
        /// (and any other class's) in place. Omit to clear every mark for the
        /// account.
        #[arg(long, value_name = "CLASS")]
        model_class: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn handle_api_keys_command(action: ApiKeysAction, workspace: &str) -> Result<()> {
    let workspace = resolve_tokens_workspace(workspace)?;
    match action {
        ApiKeysAction::Add {
            provider,
            name,
            key_file,
            env_var,
            shared,
            force,
            max_concurrent,
            json,
        } => {
            let root = if shared {
                loom_daemon::api_keys_pool::paths::shared_api_keys_dir().context(
                    "the shared machine-level pool is disabled (LOOM_SHARED_API_KEYS_DIR is empty)",
                )?
            } else {
                per_repo_api_keys_dir(&workspace)
            };
            let env_var = env_var.unwrap_or_else(|| default_env_name(&provider));
            let secret = read_key(key_file.as_deref())?;
            registry::add(&root, &provider, &name, &env_var, &secret, force)
                .map_err(anyhow::Error::msg)?;
            // After `add`, so a rejected cap cannot leave a registered key
            // behind with a half-applied declaration.
            if max_concurrent.is_some() {
                limits::set_max_concurrent(&root, &provider, &name, max_concurrent)
                    .map_err(anyhow::Error::msg)?;
            }
            if !shared {
                note_shadowed_shared_accounts(&workspace, &provider);
            }
            print_account(&registry::describe_account(&root, &provider, &name), json)
        }
        ApiKeysAction::List { provider, json } => {
            // `?`: an unreadable pool is reported as such, never as "no
            // accounts registered".
            let accounts = select::list_accounts(&workspace, provider.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&accounts)?);
            } else if accounts.is_empty() {
                // Exit 0: "no pool on this host" is a state, not a failure.
                println!(
                    "No API-key accounts registered{} in {}.",
                    provider.map_or(String::new(), |p| format!(" for provider {p:?}")),
                    describe_roots(&workspace)
                );
            } else {
                for account in &accounts {
                    print_account(account, false)?;
                }
            }
            Ok(())
        }
        ApiKeysAction::Disable {
            provider,
            name,
            json,
        } => set_enabled(&workspace, &provider, &name, false, json),
        ApiKeysAction::Enable {
            provider,
            name,
            json,
        } => set_enabled(&workspace, &provider, &name, true, json),
        ApiKeysAction::Remove {
            provider,
            name,
            json,
        } => {
            let root = provider_root(&workspace, &provider)?;
            registry::remove(&root, &provider, &name).map_err(anyhow::Error::msg)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"provider": provider, "name": name, "removed": true})
                );
            } else {
                println!("Removed {provider}/{name} from {}.", root.display());
            }
            Ok(())
        }
        ApiKeysAction::Limit {
            provider,
            name,
            max_concurrent,
            unlimited,
            json,
        } => {
            if max_concurrent.is_none() && !unlimited {
                bail!("pass --max-concurrent <N> to declare a cap, or --unlimited to clear it");
            }
            let root = provider_root(&workspace, &provider)?;
            limits::set_max_concurrent(&root, &provider, &name, max_concurrent)
                .map_err(anyhow::Error::msg)?;
            print_account(&registry::describe_account(&root, &provider, &name), json)
        }
        ApiKeysAction::Health { provider, json } => {
            let snapshot = select::health(&workspace, provider.as_deref())?;
            let unreadable: Vec<&str> = snapshot
                .iter()
                .filter(|p| p.unreadable.is_some())
                .map(|p| p.provider.as_str())
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else if snapshot.is_empty() {
                println!("No API-key pool on this host ({}).", describe_roots(&workspace));
            }
            for provider_health in snapshot.iter().filter(|_| !json) {
                if let Some(problem) = &provider_health.unreadable {
                    println!("{}: UNREADABLE — {problem}", provider_health.provider);
                    continue;
                }
                println!(
                    "{}: {}/{} selectable ({} disabled, {} malformed, {} exhausted, {} at \
                     concurrency cap, {} unverifiable) in {}",
                    provider_health.provider,
                    provider_health.selectable,
                    provider_health.total,
                    provider_health.disabled,
                    provider_health.malformed,
                    provider_health.exhausted,
                    provider_health.at_capacity,
                    provider_health.unverifiable,
                    provider_health.dir.display(),
                );
                if !provider_health.pinned.is_empty() {
                    println!("  pinned: {}", provider_health.pinned.join(", "));
                }
                if !provider_health.insecure_permissions.is_empty() {
                    println!(
                        "  WARNING permissions looser than 0600: {}",
                        provider_health.insecure_permissions.join(", ")
                    );
                }
                for account in &provider_health.accounts {
                    print_account(account, false)?;
                }
            }
            // A pool that cannot be read is a failed health check, not a
            // clean bill of health with zero accounts.
            if !unreadable.is_empty() {
                bail!("unreadable API-key pool for provider(s): {}", unreadable.join(", "));
            }
            Ok(())
        }
        ApiKeysAction::MarkBad {
            provider,
            name,
            reason,
            cooldown_secs,
            model_class,
            json,
        } => {
            let root = provider_root(&workspace, &provider)?;
            let mark = bad_marks::mark_bad_for_class(
                &root,
                &provider,
                &name,
                &reason,
                cooldown_secs,
                model_class.as_deref(),
            )
            .map_err(anyhow::Error::msg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&mark)?);
            } else {
                println!(
                    "Marked {provider}/{name}{} bad{}.",
                    mark.class_suffix(),
                    mark.resets_at
                        .map_or(" indefinitely (until `unblock`)".to_string(), |secs| {
                            format!(" until unix {secs}")
                        })
                );
            }
            Ok(())
        }
        ApiKeysAction::Unblock {
            provider,
            name,
            model_class,
            json,
        } => {
            let root = provider_root(&workspace, &provider)?;
            bad_marks::unmark_for_class(&root, &provider, &name, model_class.as_deref())
                .map_err(anyhow::Error::msg)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "provider": provider,
                        "name": name,
                        "modelClass": model_class,
                        "unblocked": true,
                    })
                );
            } else {
                println!(
                    "Unblocked {provider}/{name}{}.",
                    model_class.map_or(String::new(), |class| format!(" [model-class:{class}]"))
                );
            }
            Ok(())
        }
    }
}

fn set_enabled(
    workspace: &std::path::Path,
    provider: &str,
    name: &str,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let root = provider_root(workspace, provider)?;
    let account =
        registry::set_enabled(&root, provider, name, enabled).map_err(anyhow::Error::msg)?;
    print_account(&account, json)
}

/// The effective pool root for one provider (per-repo if it holds accounts for
/// that provider, else the shared pool). Validates first so a malformed
/// provider name is never joined onto a path.
fn provider_root(workspace: &std::path::Path, provider: &str) -> Result<PathBuf> {
    loom_daemon::api_keys_pool::paths::validate_provider(provider).map_err(anyhow::Error::msg)?;
    Ok(resolve_provider_root(workspace, provider)?)
}

fn describe_roots(workspace: &std::path::Path) -> String {
    pool_roots(workspace)
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Registering a per-repo account takes that provider over for this repo, so
/// say so when it has just put shared accounts out of reach.
fn note_shadowed_shared_accounts(workspace: &std::path::Path, provider: &str) {
    let shadowed = pool_roots(workspace)
        .iter()
        .skip(1)
        .filter_map(|shared| registry::list_provider(shared, provider).ok())
        .map(|accounts| accounts.len())
        .sum::<usize>();
    if shadowed > 0 {
        eprintln!(
            "note: this repo's own {provider:?} accounts now take precedence here; {shadowed} \
             account(s) in the shared pool are not used from this repo."
        );
    }
}

/// Render one account. Cannot print key material: [`ApiKeyAccount`] does not
/// carry it.
fn print_account(account: &ApiKeyAccount, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(account)?);
        return Ok(());
    }
    println!(
        "{}/{}: {} (variable={}, permissions={}{}{})",
        account.provider,
        account.name,
        match &account.ineligible {
            None => "selectable",
            Some(registry::Ineligible::Disabled) => "disabled",
            Some(registry::Ineligible::Exhausted) => "exhausted",
            Some(registry::Ineligible::AtCapacity) => "at concurrency cap",
            Some(registry::Ineligible::Malformed) => "unusable",
            Some(registry::Ineligible::Unverifiable) => "withheld",
        },
        account.env_name.as_deref().unwrap_or("-"),
        if account.permissions_ok {
            "0600"
        } else {
            "TOO OPEN"
        },
        account
            .max_concurrent
            .map_or(String::new(), |cap| format!(", maxConcurrent={cap}")),
        account
            .problem
            .as_ref()
            .map_or(String::new(), |p| format!(", problem={p}")),
    );
    Ok(())
}

/// Read key material from a file or stdin. Never from argv.
fn read_key(key_file: Option<&std::path::Path>) -> Result<String> {
    let raw = match key_file {
        Some(path) if path != std::path::Path::new("-") => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read key file {}", path.display()))?,
        _ => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("cannot read key material from stdin")?;
            buffer
        }
    };
    // A key file may legitimately be a one-line `.env` fragment; accept either
    // `KEY=value` or a bare value, but never echo what we could not parse.
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("no key material supplied (pass --key-file <path>, or pipe the key on stdin)");
    }
    if trimmed.lines().count() > 1 {
        bail!("key material must be a single line");
    }
    Ok(trimmed
        .split_once('=')
        // The strict `UPPER_SNAKE_CASE` heuristic, shared with the registry's
        // own parser: a lowercase/mixed-case prefix is part of the secret
        // (base64 padding, …), so `abc=def=ghi` is never truncated to `def=ghi`.
        .filter(|(key, _)| is_conventional_env_name(key.trim()))
        .map_or(trimmed, |(_, value)| value.trim())
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_key_accepts_a_bare_value_or_an_env_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare");
        std::fs::write(&bare, "  fake-secret-value\n").unwrap();
        assert_eq!(read_key(Some(&bare)).unwrap(), "fake-secret-value");

        let fragment = dir.path().join("fragment.env");
        std::fs::write(&fragment, "ZAI_API_KEY=fake-secret-value\n").unwrap();
        assert_eq!(read_key(Some(&fragment)).unwrap(), "fake-secret-value");

        // A value that merely contains '=' is not mistaken for an assignment.
        let padded = dir.path().join("padded");
        std::fs::write(&padded, "abc=def=ghi\n").unwrap();
        assert_eq!(read_key(Some(&padded)).unwrap(), "abc=def=ghi");
    }

    #[test]
    fn read_key_rejects_empty_and_multiline_input() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "   \n").unwrap();
        assert!(read_key(Some(&empty)).is_err());
        let multi = dir.path().join("multi");
        std::fs::write(&multi, "one\ntwo\n").unwrap();
        assert!(read_key(Some(&multi))
            .unwrap_err()
            .to_string()
            .contains("single line"));
    }

    #[test]
    fn read_key_error_never_echoes_the_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let multi = dir.path().join("multi");
        std::fs::write(&multi, "fake-secret-one\nfake-secret-two\n").unwrap();
        let error = read_key(Some(&multi)).unwrap_err().to_string();
        assert!(!error.contains("fake-secret-one"), "{error}");
    }
}
