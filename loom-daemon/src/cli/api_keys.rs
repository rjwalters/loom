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
    registry, select, sync, ApiKeyAccount,
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
        /// List the shared machine-level pool instead of what this repo
        /// resolves to. This is how shared accounts a per-repo pool shadows
        /// are seen at all (issue #8450 item 1).
        #[arg(long)]
        shared: bool,
        #[arg(long)]
        json: bool,
    },
    /// Make an account ineligible for selection without removing its key.
    Disable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long, help = SHARED_VERB_HELP)]
        shared: bool,
        #[arg(long)]
        json: bool,
    },
    /// Restore an account's eligibility.
    Enable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long, help = SHARED_VERB_HELP)]
        shared: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete an account's credential file from this host.
    Remove {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long, help = SHARED_VERB_HELP)]
        shared: bool,
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
        #[arg(long, help = SHARED_VERB_HELP)]
        shared: bool,
        #[arg(long)]
        json: bool,
    },
    /// Converge this host's pool on an operator-maintained external secret
    /// source (issue #8511) — how an ephemeral or autoscaled worker that nobody
    /// runs `add` on self-registers its accounts.
    ///
    /// The source is pulled, parsed in full, and only then written: an
    /// unreachable source or one malformed line exits non-zero and leaves the
    /// pool exactly as it was. Accounts already matching the source are not
    /// rewritten, so their `.disabled`/`.allowlist`/bad-mark state survives.
    Sync {
        /// Source of truth. `cmd:<command>` runs `<command>` and reads
        /// `provider/account<TAB>KEY=value` lines from its **stdout** — which
        /// covers `aws ssm get-parameters-by-path --with-decryption`, Vault,
        /// the 1Password CLI, `age -d`, … A bare string with no `<scheme>:`
        /// prefix is run as a command too. Key material is read off the pipe,
        /// never from this argument.
        #[arg(long = "from", value_name = "SOURCE")]
        from: String,
        /// Converge the shared machine-level pool (`~/.loom/api-keys`, or
        /// `LOOM_SHARED_API_KEYS_DIR`) instead of this repo's.
        #[arg(long)]
        shared: bool,
        /// Remove accounts the source no longer lists. Scoped to the providers
        /// the source mentions, so a hand-registered account for a provider the
        /// source says nothing about is never deleted.
        #[arg(long)]
        prune: bool,
        /// Report what would change, by account name, and write nothing.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Per-provider pool health: totals, selectable count, and why each
    /// excluded account is excluded.
    Health {
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        /// Report the shared machine-level pool instead of what this repo
        /// resolves to — including providers a per-repo pool shadows.
        #[arg(long)]
        shared: bool,
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
        #[arg(long, help = SHARED_VERB_HELP)]
        shared: bool,
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
        #[arg(long, help = SHARED_VERB_HELP)]
        shared: bool,
        #[arg(long)]
        json: bool,
    },
}

/// `--shared` help shared by every verb that names one account (issue #8450
/// item 1). `add`'s own `--shared` has its own text: there it chooses where to
/// *write*, here it chooses which pool to *act on*.
const SHARED_VERB_HELP: &str = "Act on the shared machine-level pool \
    (`~/.loom/api-keys`, or `LOOM_SHARED_API_KEYS_DIR`) rather than on \
    whichever pool this repo resolves to for the provider. Required to manage \
    a shared account once this repo registers its own accounts for the same \
    provider — those shadow the shared ones for the repo.";

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
            let root = write_root(&workspace, shared)?;
            let (declared_name, secret) = read_key(key_file.as_deref())?;
            // The key file's own `NAME=value` declaration wins when `--env-var`
            // is absent — the file already has the right answer for a
            // `credentialPool`-named pool (`zai-metered`'s file says
            // `ZAI_API_KEY=...`, not `ZAI_METERED_API_KEY=...`). A disagreement
            // between an explicit `--env-var` and the file's own name is
            // refused rather than silently preferring one — neither side's
            // value is ever echoed, only the two names (issue #8450 item 3).
            let env_var = match (&env_var, &declared_name) {
                (Some(explicit), Some(declared)) if explicit != declared => bail!(
                    "--env-var {explicit:?} disagrees with the variable name the key file \
                     declares ({declared:?}); pass just one — omit --env-var to use the file's \
                     name, or fix the key file to match --env-var"
                ),
                (Some(explicit), _) => explicit.clone(),
                (None, Some(declared)) => declared.clone(),
                (None, None) => default_env_name(&provider),
            };
            registry::add(&root, &provider, &name, &env_var, &secret, force)
                .map_err(anyhow::Error::msg)?;
            // After `add`, so a rejected cap cannot leave a registered key
            // behind with a half-applied declaration.
            if max_concurrent.is_some() {
                limits::set_max_concurrent(&root, &provider, &name, max_concurrent)
                    .map_err(anyhow::Error::msg)?;
            }
            let account = registry::describe_account(&root, &provider, &name);
            // A registration `add` itself cannot describe as usable must not
            // exit 0 — e.g. a bare all-uppercase base32 key gets its `=`
            // padding split off and only the padding is stored (issue #8450
            // item 4). Remove the file rather than leave a silently destroyed
            // key behind.
            if account.ineligible == Some(registry::Ineligible::Malformed) {
                let _ = registry::remove(&root, &provider, &name);
                bail!(
                    "registered account {provider}/{name} is not usable ({}); removed it \
                     rather than leave a broken registration behind. Check the key material \
                     and retry.",
                    account.problem.as_deref().unwrap_or("malformed")
                );
            }
            if !shared {
                note_shadowed_shared_accounts(&workspace, &provider);
            }
            print_account(&account, json)
        }
        ApiKeysAction::List {
            provider,
            shared,
            json,
        } => {
            let roots = acting_roots(&workspace, shared)?;
            // `?`: an unreadable pool is reported as such, never as "no
            // accounts registered".
            let accounts = select::list_accounts_in(&roots, provider.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&accounts)?);
            } else if accounts.is_empty() {
                // Exit 0: "no pool on this host" is a state, not a failure.
                println!(
                    "No API-key accounts registered{} in {}.",
                    provider.map_or(String::new(), |p| format!(" for provider {p:?}")),
                    describe_root_list(&roots)
                );
            } else {
                for account in &accounts {
                    print_account(account, false)?;
                }
                if !shared {
                    note_shadowed_shared_providers(&workspace);
                }
            }
            Ok(())
        }
        ApiKeysAction::Disable {
            provider,
            name,
            shared,
            json,
        } => set_enabled(&workspace, &provider, &name, false, shared, json),
        ApiKeysAction::Enable {
            provider,
            name,
            shared,
            json,
        } => set_enabled(&workspace, &provider, &name, true, shared, json),
        ApiKeysAction::Remove {
            provider,
            name,
            shared,
            json,
        } => {
            let root = provider_root(&workspace, &provider, shared)?;
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
            shared,
            json,
        } => {
            if max_concurrent.is_none() && !unlimited {
                bail!("pass --max-concurrent <N> to declare a cap, or --unlimited to clear it");
            }
            let root = provider_root(&workspace, &provider, shared)?;
            limits::set_max_concurrent(&root, &provider, &name, max_concurrent)
                .map_err(anyhow::Error::msg)?;
            print_account(&registry::describe_account(&root, &provider, &name), json)
        }
        ApiKeysAction::Sync {
            from,
            shared,
            prune,
            dry_run,
            json,
        } => {
            let root = write_root(&workspace, shared)?;
            let outcome = sync::sync(&sync::SyncOptions {
                root: &root,
                source: &from,
                prune,
                dry_run,
            })
            .map_err(anyhow::Error::msg)?;
            if !shared {
                for provider in synced_providers(&outcome) {
                    note_shadowed_shared_accounts(&workspace, &provider);
                }
            }
            print_sync_outcome(&outcome, json)
        }
        ApiKeysAction::Health {
            provider,
            shared,
            json,
        } => {
            let roots = acting_roots(&workspace, shared)?;
            let snapshot = select::health_in(&roots, provider.as_deref())?;
            let unreadable: Vec<String> = snapshot
                .iter()
                .filter(|p| p.unreadable.is_some())
                .map(|p| p.provider.clone())
                .chain(
                    snapshot
                        .iter()
                        .filter(|p| p.pin_unreadable.is_some())
                        .map(|p| format!("{} (pin)", p.provider)),
                )
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else if snapshot.is_empty() {
                println!("No API-key pool on this host ({}).", describe_root_list(&roots));
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
                if let Some(problem) = &provider_health.pin_unreadable {
                    println!("  pin: UNREADABLE — {problem}");
                } else if !provider_health.pinned.is_empty() {
                    println!("  pinned: {}", provider_health.pinned.join(", "));
                }
                if let Some(state) = &provider_health.last_sync {
                    println!("  {}", describe_last_sync(state));
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
            if !json && !shared {
                note_shadowed_shared_providers(&workspace);
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
            shared,
            json,
        } => {
            let root = provider_root(&workspace, &provider, shared)?;
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
            shared,
            json,
        } => {
            let root = provider_root(&workspace, &provider, shared)?;
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
    shared: bool,
    json: bool,
) -> Result<()> {
    let root = provider_root(workspace, provider, shared)?;
    let account =
        registry::set_enabled(&root, provider, name, enabled).map_err(anyhow::Error::msg)?;
    print_account(&account, json)
}

/// The root a *writing* verb (`add`, `sync`) targets: the shared machine-level
/// pool with `--shared`, this repo's otherwise. Unlike [`provider_root`] this
/// never depends on what is already registered — it is where new accounts go.
fn write_root(workspace: &std::path::Path, shared: bool) -> Result<PathBuf> {
    if shared {
        loom_daemon::api_keys_pool::paths::shared_api_keys_dir().context(
            "the shared machine-level pool is disabled (LOOM_SHARED_API_KEYS_DIR is empty)",
        )
    } else {
        Ok(per_repo_api_keys_dir(workspace))
    }
}

/// The providers a sync touched, de-duplicated — derived from the plan's
/// `provider/account` identifiers, which are the only account-shaped strings a
/// [`sync::SyncOutcome`] carries.
fn synced_providers(outcome: &sync::SyncOutcome) -> Vec<String> {
    let mut providers: Vec<String> = outcome
        .plan
        .added
        .iter()
        .chain(&outcome.plan.updated)
        .chain(&outcome.plan.unchanged)
        .filter_map(|id| id.split_once('/').map(|(provider, _)| provider.to_string()))
        .collect();
    providers.sort();
    providers.dedup();
    providers
}

/// Render a sync result. Cannot print key material: [`sync::SyncOutcome`]
/// carries account *names* and counts only.
fn print_sync_outcome(outcome: &sync::SyncOutcome, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcome)?);
        return Ok(());
    }
    let plan = &outcome.plan;
    println!(
        "{} {}: {} to add, {} to update, {} to remove, {} unchanged ({}).",
        if outcome.dry_run {
            "Would sync"
        } else {
            "Synced"
        },
        outcome.root.display(),
        plan.added.len(),
        plan.updated.len(),
        plan.removed.len(),
        plan.unchanged.len(),
        if outcome.pruned {
            "--prune: accounts the source no longer lists are removed, for the providers it names"
        } else {
            "no --prune: accounts the source no longer lists are kept"
        },
    );
    for (marker, ids) in [
        ("+", &plan.added),
        ("~", &plan.updated),
        ("-", &plan.removed),
    ] {
        for id in ids {
            println!("  {marker} {id}");
        }
    }
    if plan.is_noop() && !outcome.dry_run {
        println!("  (already converged — no file was rewritten)");
    }
    Ok(())
}

/// One `health` line for the last successful sync of a pool root, or `None` on
/// a host that has never synced (where the line would be noise).
fn describe_last_sync(state: &loom_daemon::api_keys_pool::SyncState) -> String {
    let when = chrono::DateTime::from_timestamp(
        i64::try_from(state.last_success_at).unwrap_or(i64::MAX),
        0,
    )
    .map_or_else(
        || format!("unix {}", state.last_success_at),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    let age = chrono::Utc::now().timestamp() - i64::try_from(state.last_success_at).unwrap_or(0);
    format!(
        "last sync: {when} ({}) from {} — {} account(s)",
        humanize_age(age),
        state.source,
        state.accounts
    )
}

/// `"3d 4h ago"`. Coarse on purpose: the question this answers is "has this
/// host's sync been failing for days", not "how many seconds".
fn humanize_age(seconds: i64) -> String {
    if seconds < 0 {
        return "in the future — check this host's clock".to_string();
    }
    let (days, hours, minutes) =
        (seconds / 86_400, (seconds % 86_400) / 3_600, (seconds % 3_600) / 60);
    if days > 0 {
        format!("{days}d {hours}h ago")
    } else if hours > 0 {
        format!("{hours}h {minutes}m ago")
    } else {
        format!("{minutes}m ago")
    }
}

/// The pool root a verb acts on for one provider: with `shared`, the shared
/// machine-level pool outright; otherwise the effective root (per-repo if it
/// holds accounts for that provider, else the shared pool).
///
/// `--shared` is what makes a shadowed shared account reachable at all (issue
/// #8450 item 1): once a repo registers its own accounts for a provider, the
/// ordinary resolution below can no longer name the shared ones, and the only
/// workaround was to re-run the verb with `--workspace <dir with no pool>`.
///
/// Validates the provider name first so a malformed one is never joined onto a
/// path.
fn provider_root(workspace: &std::path::Path, provider: &str, shared: bool) -> Result<PathBuf> {
    loom_daemon::api_keys_pool::paths::validate_provider(provider).map_err(anyhow::Error::msg)?;
    if shared {
        return shared_root();
    }
    Ok(resolve_provider_root(workspace, provider)?)
}

/// The precedence-ordered roots a `list`/`health` pass reads: just the shared
/// machine-level pool with `--shared`, else everything this repo resolves over.
fn acting_roots(workspace: &std::path::Path, shared: bool) -> Result<Vec<PathBuf>> {
    if shared {
        return Ok(vec![shared_root()?]);
    }
    Ok(pool_roots(workspace))
}

/// The shared machine-level pool root, or a diagnosable error when it is
/// disabled — never a silent fall back to the per-repo pool, which would make
/// `--shared` act on exactly the pool the operator was trying to reach past.
fn shared_root() -> Result<PathBuf> {
    loom_daemon::api_keys_pool::paths::shared_api_keys_dir()
        .context("the shared machine-level pool is disabled (LOOM_SHARED_API_KEYS_DIR is empty)")
}

fn describe_root_list(roots: &[PathBuf]) -> String {
    roots
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
             account(s) in the shared pool are not used from this repo. Manage them with \
             `--shared`."
        );
    }
}

/// After a `list`/`health` pass that used ordinary precedence, say which
/// providers had shared accounts hidden by this repo's own pool, and how to
/// reach them (issue #8450 item 1). Without this, a shadowed shared account is
/// invisible: it appears in no output and the failure mode is a confusing
/// `no such account <provider>/<name>` from a later management verb.
///
/// Best-effort and secret-free — a root that cannot be read is simply not
/// counted here; `list`/`health` already fail loudly on that themselves.
fn note_shadowed_shared_providers(workspace: &std::path::Path) {
    let roots = pool_roots(workspace);
    let Some((per_repo, shared_roots)) = roots.split_first() else {
        return;
    };
    let shadowed: Vec<String> = shared_roots
        .iter()
        .filter_map(|root| loom_daemon::api_keys_pool::paths::list_providers(root).ok())
        .flatten()
        .filter(|provider| {
            // Shadowed exactly when the per-repo pool holds accounts for the
            // same provider — the precedence rule `resolve_provider_root`
            // applies, asked about one provider at a time.
            loom_daemon::api_keys_pool::paths::list_account_files(
                &loom_daemon::api_keys_pool::paths::provider_dir(per_repo, provider),
            )
            .is_ok_and(|files| !files.is_empty())
        })
        .collect();
    if !shadowed.is_empty() {
        let mut unique = shadowed;
        unique.sort();
        unique.dedup();
        eprintln!(
            "note: this repo's own pool shadows the shared machine-level pool for provider(s) \
             {}; add --shared to see or manage the shared accounts.",
            unique.join(", ")
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
///
/// Returns `(declared name, value)`: when the material is a `NAME=value`
/// fragment (the strict `UPPER_SNAKE_CASE` heuristic below), the declared name
/// is handed back so `add` can honour it instead of discarding it (issue
/// #8450 item 3) — a bare value has no declared name.
fn read_key(key_file: Option<&std::path::Path>) -> Result<(Option<String>, String)> {
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
    //
    // Blank and `#`-comment lines are dropped first (issue #8450, operator
    // follow-up): an operator's stored copy of a key is very often a small env
    // file — a few `#` lines recording which account it is and where to rotate
    // it, then exactly one assignment. The registry's own parser
    // (`registry::parse_account`) already skips those lines when *reading* an
    // account file, so rejecting them here made `add` stricter about its input
    // than the pool is about its storage, and every operator had to rediscover
    // the `grep '^VAR=' <file> | loom-daemon api-keys add …` workaround. The
    // "one account = one assignment" contract is unchanged: anything with more
    // than one remaining line is still refused, and nothing read is echoed.
    let significant: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    if significant.is_empty() {
        bail!("no key material supplied (pass --key-file <path>, or pipe the key on stdin)");
    }
    if significant.len() > 1 {
        bail!(
            "key material must be a single line (blank and '#' comment lines are ignored; \
             {} assignment/value lines found)",
            significant.len()
        );
    }
    let trimmed = significant[0];
    // The strict `UPPER_SNAKE_CASE` heuristic, shared with the registry's own
    // parser: a lowercase/mixed-case prefix is part of the secret (base64
    // padding, …), so `abc=def=ghi` is never mistaken for a declared name.
    match trimmed
        .split_once('=')
        .filter(|(key, _)| is_conventional_env_name(key.trim()))
    {
        Some((key, value)) => Ok((Some(key.trim().to_string()), value.trim().to_string())),
        None => Ok((None, trimmed.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_key_accepts_a_bare_value_or_an_env_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare");
        std::fs::write(&bare, "  fake-secret-value\n").unwrap();
        assert_eq!(read_key(Some(&bare)).unwrap(), (None, "fake-secret-value".to_string()));

        let fragment = dir.path().join("fragment.env");
        std::fs::write(&fragment, "ZAI_API_KEY=fake-secret-value\n").unwrap();
        assert_eq!(
            read_key(Some(&fragment)).unwrap(),
            (Some("ZAI_API_KEY".to_string()), "fake-secret-value".to_string())
        );

        // A value that merely contains '=' is not mistaken for an assignment.
        let padded = dir.path().join("padded");
        std::fs::write(&padded, "abc=def=ghi\n").unwrap();
        assert_eq!(read_key(Some(&padded)).unwrap(), (None, "abc=def=ghi".to_string()));
    }

    /// Issue #8450 (operator follow-up): the most natural operator key file is
    /// a small commented env file. `add --key-file` must accept it rather than
    /// force the `grep '^VAR=' <file> | …` workaround — the registry's own
    /// parser already skips blank and `#` lines when reading an account file.
    #[test]
    fn read_key_ignores_blank_and_comment_lines_in_an_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let commented = dir.path().join("zai.env");
        std::fs::write(
            &commented,
            "# Z.ai coding plan, account alpha\n# rotate at https://example.invalid/keys\n\n\
             ZAI_API_KEY=fake-secret-value\n\n",
        )
        .unwrap();
        assert_eq!(
            read_key(Some(&commented)).unwrap(),
            (Some("ZAI_API_KEY".to_string()), "fake-secret-value".to_string())
        );

        // A bare value in an otherwise-commented file works too.
        let bare = dir.path().join("bare.env");
        std::fs::write(&bare, "# which account\n\nfake-secret-value\n").unwrap();
        assert_eq!(read_key(Some(&bare)).unwrap(), (None, "fake-secret-value".to_string()));
    }

    #[test]
    fn read_key_rejects_empty_and_multiline_input() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "   \n").unwrap();
        assert!(read_key(Some(&empty)).is_err());
        // A file with nothing but comments is still "no key material".
        let comments_only = dir.path().join("comments");
        std::fs::write(&comments_only, "# just a note\n\n# another\n").unwrap();
        assert!(read_key(Some(&comments_only))
            .unwrap_err()
            .to_string()
            .contains("no key material"));
        let multi = dir.path().join("multi");
        std::fs::write(&multi, "one\ntwo\n").unwrap();
        assert!(read_key(Some(&multi))
            .unwrap_err()
            .to_string()
            .contains("single line"));
        // Two assignments is still "one account = one assignment" violated,
        // even with comments interleaved.
        let two_assignments = dir.path().join("two.env");
        std::fs::write(
            &two_assignments,
            "# two accounts in one file\nZAI_API_KEY=fake-one\nOPENAI_API_KEY=fake-two\n",
        )
        .unwrap();
        let error = read_key(Some(&two_assignments)).unwrap_err().to_string();
        assert!(error.contains("single line"), "{error}");
        assert!(!error.contains("fake-one"), "{error}");
    }

    #[test]
    fn last_sync_renders_an_age_coarse_enough_to_read_at_a_glance() {
        assert_eq!(humanize_age(0), "0m ago");
        assert_eq!(humanize_age(90), "1m ago");
        assert_eq!(humanize_age(3 * 3_600 + 12 * 60), "3h 12m ago");
        assert_eq!(humanize_age(3 * 86_400 + 4 * 3_600), "3d 4h ago");
        // A host whose clock ran backwards must say so rather than print a
        // nonsense age that reads as "synced moments ago".
        assert!(humanize_age(-5).contains("clock"));
    }

    #[test]
    fn the_health_sync_line_names_the_source_and_the_time() {
        let state = loom_daemon::api_keys_pool::SyncState {
            last_success_at: 1_700_000_000,
            source: "cmd:/opt/loom/fetch-keys.sh".to_string(),
            accounts: 3,
        };
        let line = describe_last_sync(&state);
        assert!(line.contains("2023-11-14T22:13:20Z"), "{line}");
        assert!(line.contains("cmd:/opt/loom/fetch-keys.sh"), "{line}");
        assert!(line.contains("3 account(s)"), "{line}");
    }

    #[test]
    fn read_key_error_never_echoes_the_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let multi = dir.path().join("multi");
        std::fs::write(&multi, "fake-secret-one\nfake-secret-two\n").unwrap();
        let error = read_key(Some(&multi)).unwrap_err().to_string();
        assert!(!error.contains("fake-secret-one"), "{error}");
    }

    fn add_action(
        provider: &str,
        name: &str,
        key_file: &std::path::Path,
        env_var: Option<&str>,
    ) -> ApiKeysAction {
        ApiKeysAction::Add {
            provider: provider.to_string(),
            name: name.to_string(),
            key_file: Some(key_file.to_path_buf()),
            env_var: env_var.map(str::to_string),
            shared: false,
            force: false,
            max_concurrent: None,
            json: false,
        }
    }

    /// Issue #8450 item 1: once a repo registers its own accounts for a
    /// provider, the shared machine-level accounts for that same provider are
    /// shadowed. Before `--shared` existed on the management verbs they could
    /// not be named at all from that repo — `disable zai shared-one` answered
    /// `no such account`, and the only workaround was `--workspace <a dir with
    /// no pool>`. `--shared` must reach them for both reading and writing.
    #[test]
    #[serial_test::serial]
    fn shared_accounts_shadowed_by_a_per_repo_pool_are_reachable_with_shared() {
        use loom_daemon::api_keys_pool::paths::SHARED_API_KEYS_DIR_ENV;

        let shared_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_API_KEYS_DIR_ENV, shared_dir.path().to_str().unwrap());

        let shared_root = shared_dir.path().to_path_buf();
        let repo_root = per_repo_api_keys_dir(repo.path());
        // Same provider in both pools: the per-repo one takes "loomtest" over
        // for this repo, shadowing the shared account entirely.
        registry::add(&shared_root, "loomtest", "shared-one", "LOOMTEST_API_KEY", "fake-a", false)
            .unwrap();
        registry::add(&repo_root, "loomtest", "local-one", "LOOMTEST_API_KEY", "fake-b", false)
            .unwrap();
        let workspace = repo.path().to_str().unwrap();

        // Baseline: ordinary precedence cannot name the shared account.
        let shadowed = handle_api_keys_command(
            ApiKeysAction::Disable {
                provider: "loomtest".into(),
                name: "shared-one".into(),
                shared: false,
                json: false,
            },
            workspace,
        );
        assert!(shadowed.is_err(), "the shared account must be shadowed without --shared");

        // `list --shared` sees it...
        let listed =
            select::list_accounts_in(std::slice::from_ref(&shared_root), Some("loomtest")).unwrap();
        assert_eq!(listed.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), vec!["shared-one"]);
        // ...and `disable --shared` manages it, without touching the per-repo
        // account of the same provider.
        handle_api_keys_command(
            ApiKeysAction::Disable {
                provider: "loomtest".into(),
                name: "shared-one".into(),
                shared: true,
                json: false,
            },
            workspace,
        )
        .unwrap();
        assert_eq!(
            registry::describe_account(&shared_root, "loomtest", "shared-one").ineligible,
            Some(registry::Ineligible::Disabled)
        );
        assert_eq!(
            registry::describe_account(&repo_root, "loomtest", "local-one").ineligible,
            None,
            "--shared must not reach into the per-repo pool"
        );

        std::env::remove_var(SHARED_API_KEYS_DIR_ENV);
    }

    /// `--shared` must never silently fall back to the per-repo pool when the
    /// shared pool is disabled — that is precisely the pool the operator was
    /// reaching *past*, so a silent fallback would act on the wrong account.
    #[test]
    #[serial_test::serial]
    fn shared_refuses_rather_than_falling_back_when_the_shared_pool_is_disabled() {
        use loom_daemon::api_keys_pool::paths::SHARED_API_KEYS_DIR_ENV;

        let repo = tempfile::tempdir().unwrap();
        std::env::set_var(SHARED_API_KEYS_DIR_ENV, "");
        registry::add(
            &per_repo_api_keys_dir(repo.path()),
            "loomtest",
            "local-one",
            "LOOMTEST_API_KEY",
            "fake-b",
            false,
        )
        .unwrap();
        let error = handle_api_keys_command(
            ApiKeysAction::Remove {
                provider: "loomtest".into(),
                name: "local-one".into(),
                shared: true,
                json: false,
            },
            repo.path().to_str().unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("shared machine-level pool is disabled"), "{error}");
        assert!(
            registry::describe_account(
                &per_repo_api_keys_dir(repo.path()),
                "loomtest",
                "local-one"
            )
            .env_name
            .is_some(),
            "the per-repo account must be untouched"
        );
        std::env::remove_var(SHARED_API_KEYS_DIR_ENV);
    }

    /// Issue #8450 item 3: a `NAME=value` key file's own declared name is
    /// honoured when `--env-var` is absent, instead of always writing
    /// `<PROVIDER>_API_KEY=` — the case that matters for a `credentialPool`-
    /// named pool, where the derived default would be wrong by construction.
    #[test]
    fn add_honours_the_key_files_declared_name_when_env_var_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let key_file = tmp.path().join("key.env");
        std::fs::write(&key_file, "ZAI_API_KEY=fake-secret-real\n").unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        handle_api_keys_command(
            add_action("zai-metered", "acct", &key_file, None),
            workspace.to_str().unwrap(),
        )
        .unwrap();
        let root = per_repo_api_keys_dir(&workspace);
        let account = registry::describe_account(&root, "zai-metered", "acct");
        assert_eq!(account.env_name.as_deref(), Some("ZAI_API_KEY"));
        assert_eq!(account.ineligible, None, "{account:?}");
    }

    /// Issue #8450 item 3: an explicit `--env-var` that disagrees with the key
    /// file's own declared name is refused rather than silently preferring
    /// either one — and neither side's secret value is echoed.
    #[test]
    fn add_refuses_a_disagreeing_env_var_without_echoing_a_value() {
        let tmp = tempfile::tempdir().unwrap();
        let key_file = tmp.path().join("key.env");
        std::fs::write(&key_file, "OPENAI_API_KEY=fake-secret-value\n").unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let error = handle_api_keys_command(
            add_action("loomtest", "acct", &key_file, Some("ZAI_API_KEY")),
            workspace.to_str().unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ZAI_API_KEY"), "{error}");
        assert!(error.contains("OPENAI_API_KEY"), "{error}");
        assert!(!error.contains("fake-secret-value"), "{error}");
        // Refused before ever writing an account file.
        let root = per_repo_api_keys_dir(&workspace);
        assert!(registry::list_provider(&root, "loomtest")
            .unwrap()
            .is_empty());
    }

    /// Issue #8450 item 4: `add` must not exit 0 on a registration it then
    /// describes as unusable — a bare base32 key with `=` padding is split at
    /// its first `=` and only the padding survives, which `describe` reports
    /// `Malformed`. The broken file must not be left behind either.
    #[test]
    fn add_fails_and_removes_the_file_when_the_registration_is_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        let key_file = tmp.path().join("key");
        std::fs::write(&key_file, "RKFAKEBASE32KEYMATERIAL======\n").unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let error = handle_api_keys_command(
            add_action("loomtest", "acct", &key_file, None),
            workspace.to_str().unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("not usable"), "{error}");
        let root = per_repo_api_keys_dir(&workspace);
        assert!(
            registry::list_provider(&root, "loomtest")
                .unwrap()
                .is_empty(),
            "a registration nobody can use must not be left behind"
        );
    }
}
