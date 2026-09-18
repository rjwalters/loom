//! `loom-daemon tokens select` — the pool's token-selection entry point.
//!
//! Extracted from `cli/tokens.rs`'s inline `TokensAction::Select` match arm
//! (issue #8146) so the parent file stays under the
//! `check-file-size-budget.sh` ratchet while this handler grows the `--role`
//! argument that feeds the prompt-cache affinity preference
//! (`tokens_pool::affinity`). Behavior is otherwise unchanged.

use anyhow::{anyhow, Result};

use super::{resolve_tokens_workspace, shell_single_quote};

/// Parsed `tokens select` arguments. A struct rather than six positional
/// parameters so the call site stays readable and a future flag cannot be
/// silently transposed with a neighbour of the same type.
pub(crate) struct SelectArgs {
    pub workspace: String,
    pub provider: String,
    pub export: bool,
    pub no_key: bool,
    pub auto_unpin: bool,
    /// Raw model alias or pinned ID this spawn will run (`--model`, issue
    /// #8058). Narrows the `.bad_tokens` skip to that model's class; `None`,
    /// or a value the classifier does not recognize, selects account-wide.
    pub model: Option<String>,
    /// The spawning role's name (`LOOM_ROLE`), when the caller knows it.
    /// Used only as the prompt-cache affinity key (issue #8146); `None` —
    /// the default, and every pre-#8146 caller — selects exactly as before.
    pub role: Option<String>,
}

/// Handle `loom-daemon tokens select`.
///
/// # Errors
/// Returns an error for an unparseable `--provider`, an unresolvable
/// `--workspace`, or a failed Codex account selection. An empty Claude pool
/// exits the process with `EX_CONFIG` (78) rather than returning, preserving
/// the exit code `spawn-claude.sh` keys on.
pub(crate) fn handle_select(args: SelectArgs) -> Result<()> {
    use loom_daemon::tokens_pool::{bad_tokens, select};

    let SelectArgs {
        workspace,
        provider,
        export,
        no_key,
        auto_unpin,
        model,
        role,
    } = args;
    // #5609 (design D8/D9): parsed through `AccountProvider`'s
    // `FromStr` rather than compared against two hardcoded string
    // literals, so the valid vocabulary has exactly one definition
    // and the error message enumerates it from that same place.
    let provider: loom_daemon::tokens_pool::AccountProvider =
        provider.parse().map_err(|error| anyhow!("{error}"))?;
    let ws = resolve_tokens_workspace(&workspace)?;
    // Resolved workspace (issue #4948, suggested-fix option 3) —
    // always stderr so `--export`'s stdout stays eval-safe and
    // `--json`'s (non-`--export`) stdout stays a bare JSON object.
    eprintln!("Resolved workspace: {}", ws.display());
    if provider == loom_daemon::tokens_pool::AccountProvider::Codex {
        // #8277: the same `--model` this arm's Claude sibling already
        // accepts (see `select_token_for_model_and_role` below) now also
        // narrows Codex selection past a class-scoped
        // `MODEL_CREDITS_EXHAUSTED` hold (#8058 Phase 2).
        let selected = loom_daemon::tokens_pool::select_account(&ws, provider, model.as_deref())
            .map_err(|error| anyhow!(error))?;
        let directory = match &selected.binding {
            loom_daemon::tokens_pool::AccountBinding::CodexHome { directory } => directory,
            _ => unreachable!("Codex selection returned a non-Codex binding"),
        };
        if export {
            println!("export CODEX_HOME={}", shell_single_quote(&directory.display().to_string()));
            println!(
                "export LOOM_ACCOUNT_PROVIDER='{}'\nexport LOOM_ACCOUNT_NAME={}",
                selected.id.provider,
                shell_single_quote(&selected.id.name)
            );
            // #5609 AC 6: alongside the existing PROVIDER/NAME pair,
            // so a dispatched sweep can be correlated back to the
            // exact upstream account. Codex's storage backend has no
            // upstream id to carry (see `SelectedAccount::upstream_id`
            // doc comment), so this is a no-op today and becomes live
            // the day a Codex-backed `ProviderAdapter` gains one.
            if let Some(upstream_id) = &selected.upstream_id {
                println!("export LOOM_ACCOUNT_UPSTREAM_ID={}", shell_single_quote(upstream_id));
            }
            println!("LOOM_TOKEN_MODE='{}'", selected.mode);
        } else {
            println!(
                "{}",
                serde_json::json!({
                    "provider": selected.id.provider.to_string(),
                    "name": selected.id.name,
                    "upstream_id": selected.upstream_id,
                    "credential_kind": "codex_home",
                    "credential_reference": directory,
                    "mode": selected.mode,
                })
            );
        }
        return Ok(());
    }
    debug_assert_eq!(provider, loom_daemon::tokens_pool::AccountProvider::Claude);
    if auto_unpin {
        if let Some(msg) = loom_daemon::tokens_pool::maybe_auto_unpin(&ws) {
            eprintln!("{msg}");
        }
    }
    // Routine `.bad_tokens` hygiene (#4643). `cleanup_bad_tokens` had
    // zero callers in the whole tree, so pools accumulated expired
    // exhaustion entries forever (the live shared pool still held
    // days-old lines). Selection is the routine path every spawn takes,
    // and pruning is behavior-neutral — `is_bad` already ignores
    // aged-out entries — so this only bounds the file. Best-effort:
    // never let a cleanup failure block a spawn, and no lock is taken
    // at all when there is nothing to prune.
    let _ = bad_tokens::cleanup_bad_tokens(&ws, bad_tokens::DEFAULT_CLEANUP_MAX_AGE_SECS);
    // Two orthogonal narrowings, combined in one call:
    // - `--model` (#8058) narrows the `.bad_tokens` skip to the model class
    //   this spawn will actually run. `None`, or a model the classifier does
    //   not recognize, degrades to account-wide selection — selection must
    //   never fail closed on a model name it does not know.
    // - `--role` (#8146) is the prompt-cache affinity key's second half, and
    //   only ever *reorders* accounts the tiers already found eligible.
    // Both `None` is bit-for-bit the pre-#8058/#8146 `select_token` path, as
    // is any role on a pool with affinity unconfigured.
    match select::select_token_for_model_and_role(&ws, None, model.as_deref(), role.as_deref()) {
        Ok(sel) => {
            if export {
                if no_key {
                    println!(
                        "# selected={} mode={} file={}",
                        sel.name,
                        sel.mode,
                        sel.file.display()
                    );
                } else {
                    // Tokens are base64/hex-like and never contain a
                    // single quote in practice; this is a simple
                    // wrap, not a full Python repr() escape.
                    println!("export CLAUDE_CODE_OAUTH_TOKEN='{}'", sel.key);
                    // Shell-evalable (issue #4228): lets
                    // spawn-claude.sh / claude-wrapper.sh `eval` this
                    // output directly instead of round-tripping
                    // through `python3 -c 'import json...'`.
                    println!("export LOOM_TOKEN_NAME='{}'", sel.name);
                    // #5609 AC 6: alongside the existing
                    // LOOM_TOKEN_NAME, so a dispatched sweep can be
                    // correlated back to the exact upstream account.
                    // `None` for a `.token` file with no `index.json`
                    // row (fail-open pool) — never fabricated.
                    if let Some(upstream_id) = &sel.upstream_id {
                        println!("export LOOM_ACCOUNT_UPSTREAM_ID='{upstream_id}'");
                    }
                    println!("LOOM_TOKEN_MODE='{}'", sel.mode);
                    println!(
                        "# selected={} mode={} file={}",
                        sel.name,
                        sel.mode,
                        sel.file.display()
                    );
                }
            } else {
                let mut obj = serde_json::Map::new();
                obj.insert("name".to_string(), serde_json::Value::String(sel.name));
                obj.insert("provider".to_string(), serde_json::Value::String(provider.to_string()));
                obj.insert("upstream_id".to_string(), serde_json::json!(sel.upstream_id));
                obj.insert(
                    "file".to_string(),
                    serde_json::Value::String(sel.file.display().to_string()),
                );
                obj.insert("mode".to_string(), serde_json::Value::String(sel.mode.to_string()));
                if !no_key {
                    obj.insert("key".to_string(), serde_json::Value::String(sel.key));
                }
                println!("{}", serde_json::Value::Object(obj));
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(select::EX_CONFIG);
        }
    }
}
