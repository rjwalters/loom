//! Declared account → private-clone repository bindings, converged by the
//! session reconcile pass (#14111, AC1 of #14028).
//!
//! Before this, which repositories an account serves lived only in CLI state
//! (`accounts session start ACCOUNT --private-clone URL`), repeated per
//! account, per repository, per host. The key below declares them in any
//! config tier (machine defaults, fleet, repo, host-local — the same
//! [`resolve_effective_config`](crate::config_resolver::resolve_effective_config)
//! chain as `autonomous.sessionReconcile`, read from the daemon's root):
//!
//! ```json
//! { "autonomous": { "sessionReconcile": { "privateClones": {
//!     "work-account": [
//!       { "repository": "https://github.com/OWNER/REPO", "base": "main" }
//!     ]
//! } } } }
//! ```
//!
//! `base` is optional (default `main`). Each pass, once per binding:
//!
//! | Account | Action |
//! |---------|--------|
//! | no binding | nothing — not read, not touched |
//! | disabled, or held by an operator `stop` | nothing (a binding never lifts a hold) |
//! | repository already registered with that base | nothing, no `docker` call |
//! | idle, repository missing | registered by [`private_workspace::converge`] — the path `session start --private-clone` takes while idle |
//! | busy | deferred to a later pass, never interrupted, no failure counted |
//! | unknown account, non-HTTPS URL, firewalled repository, host-mounted session, conflicting base | refused, WARNed once naming the key, retried on backoff |
//!
//! Removing a binding deletes nothing: an account that still has at least one
//! binding reports every registered repository it no longer declares as
//! **drift** (WARN once per distinct set); a clone and its unfinished work stay.
//! An account whose last binding was removed behaves exactly as before.
//!
//! The key is re-read every pass, so an edit is live; no restart is needed.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::Value;

use super::{backoff_secs, AccountIndex, ReconcileState};
use crate::tokens_pool::private_workspace::{self, AccountBusy, Converged};
use crate::tokens_pool::session_hold::OperatorHeld;

/// Dotted config path of the declaration.
pub const KEY: &str = "autonomous.sessionReconcile.privateClones";
/// The base a binding gets when it names none.
pub const DEFAULT_BASE: &str = "main";

/// One declared `account → repository` binding, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub account: String,
    /// As written (HTTPS, credential-free).
    pub repository: String,
    pub base: String,
    /// Canonical repository identity ([`private_workspace::check_binding`]).
    pub key: String,
}

/// A declaration that cannot be applied; `message` names the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    /// The account the entry belongs to, when it could be told.
    pub account: Option<String>,
    pub message: String,
}

/// Everything the config declares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declared {
    pub bindings: Vec<Binding>,
    pub invalid: Vec<Invalid>,
}

/// Parse [`KEY`] out of an effective config. Malformed entries are reported in
/// [`Declared::invalid`] and never applied; the valid ones around them are.
#[must_use]
pub fn parse(effective: &Value) -> Declared {
    let mut out = Declared::default();
    let Some(block) = crate::config_resolver::get_path(effective, KEY) else {
        return out;
    };
    let Some(accounts) = block.as_object() else {
        out.invalid.push(Invalid {
            account: None,
            message: format!(
                "{KEY}: must be an object mapping account name to a list of repositories"
            ),
        });
        return out;
    };
    let mut names: Vec<&String> = accounts.keys().collect();
    names.sort();
    for account in names {
        let at = format!("{KEY}.{account}");
        let Some(entries) = accounts[account].as_array() else {
            out.invalid.push(Invalid {
                account: Some(account.clone()),
                message: format!("{at}: must be a list of {{\"repository\", \"base\"}} objects"),
            });
            continue;
        };
        let mut seen: HashMap<String, String> = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            let at = format!("{at}[{i}]");
            let invalid = |message: String| Invalid {
                account: Some(account.clone()),
                message: format!("{at}: {message}"),
            };
            let Some(repository) = entry.get("repository").and_then(Value::as_str) else {
                out.invalid
                    .push(invalid("\"repository\" (an HTTPS URL) is required".to_owned()));
                continue;
            };
            let base = match entry.get("base") {
                None | Some(Value::Null) => DEFAULT_BASE,
                Some(Value::String(base)) => base.as_str(),
                Some(_) => {
                    out.invalid
                        .push(invalid("\"base\" must be a string".to_owned()));
                    continue;
                }
            };
            let key = match private_workspace::check_binding(repository, base) {
                Ok(key) => key,
                Err(error) => {
                    out.invalid.push(invalid(format!("{error:#}")));
                    continue;
                }
            };
            if let Some(first) = seen.get(&key) {
                if first != base {
                    out.invalid.push(invalid(format!(
                        "{repository} is declared twice with different bases ({first}, {base})"
                    )));
                }
                continue;
            }
            seen.insert(key.clone(), base.to_owned());
            out.bindings.push(Binding {
                account: account.clone(),
                repository: repository.to_owned(),
                base: base.to_owned(),
                key,
            });
        }
    }
    out
}

/// Read the declarations visible from `root` (every config tier).
#[must_use]
pub fn read(root: &Path) -> Declared {
    parse(&crate::config_resolver::resolve_effective_config(root))
}

/// What one pass did about one binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Already registered; untouched.
    Present,
    /// Registered this pass.
    Registered,
    /// Not acted on (`disabled`, `held`).
    Skipped(&'static str),
    /// The account is busy; retried on a later pass.
    Deferred,
    /// Waiting out a failure's backoff.
    BackingOff { retry_at: u64 },
    /// Refused or failed; `error` names the key.
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub account: String,
    pub repository: String,
    pub outcome: Outcome,
}

/// Registered repositories an account's bindings no longer declare. Retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub account: String,
    pub undeclared: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub reports: Vec<Report>,
    pub drift: Vec<Drift>,
}

#[derive(Debug, Default)]
struct Memory {
    failures: u32,
    retry_at: u64,
    last_error: Option<String>,
}

/// Carried across passes: backoff, and what was already WARNed about.
#[derive(Debug, Default)]
pub struct State {
    memory: HashMap<(String, String), Memory>,
    invalid_reported: HashSet<String>,
    drift_reported: HashMap<String, Vec<String>>,
}

/// What a pass needs from the outside, so it can be driven without Docker.
#[allow(clippy::type_complexity)]
pub struct Inputs<'a> {
    /// Holds and `enabled=false` across every root.
    pub index: &'a AccountIndex,
    /// Make one binding true: `(binding, hold probe)`. Production routes to
    /// the root whose inventory lists the account and calls
    /// [`private_workspace::converge`].
    pub converge:
        &'a dyn Fn(&Binding, &dyn Fn() -> anyhow::Result<()>) -> anyhow::Result<Converged>,
    /// `(url, key)` of every repository the account's workspace holds; `None`:
    /// not in private-clone mode.
    pub registered: &'a dyn Fn(&str) -> anyhow::Result<Option<Vec<(String, String)>>>,
}

/// One pass over `declared`. Holds an account until its binding converges;
/// never deletes anything.
pub fn converge_pass(
    declared: &Declared,
    inputs: &Inputs<'_>,
    state: &mut ReconcileState,
    now: u64,
) -> Pass {
    let st = &mut state.bindings;
    let mut pass = Pass::default();
    for invalid in &declared.invalid {
        if st.invalid_reported.insert(invalid.message.clone()) {
            log::warn!("session_reconcile: {}; not applied", invalid.message);
        }
    }
    let index = inputs.index;
    for binding in &declared.bindings {
        let account = binding.account.as_str();
        let outcome = if index.is_disabled(account) {
            Outcome::Skipped("account disabled")
        } else if index.is_held(account) {
            Outcome::Skipped("held (operator stop)")
        } else if let Some(retry_at) = st
            .memory
            .get(&(binding.account.clone(), binding.key.clone()))
            .map(|m| m.retry_at)
            .filter(|&at| at > now)
        {
            Outcome::BackingOff { retry_at }
        } else {
            let hold = || -> anyhow::Result<()> {
                if index.is_held(account) {
                    Err(OperatorHeld.into())
                } else {
                    Ok(())
                }
            };
            let mem = st
                .memory
                .entry((binding.account.clone(), binding.key.clone()))
                .or_default();
            match (inputs.converge)(binding, &hold) {
                Ok(done) => {
                    if mem.failures > 0 {
                        log::info!(
                            "session_reconcile: {KEY}.{account}: {} converged after failures",
                            binding.repository
                        );
                    }
                    *mem = Memory::default();
                    if done == Converged::Registered {
                        log::info!(
                            "session_reconcile: {KEY}.{account}: registered {} (base {})",
                            binding.repository,
                            binding.base
                        );
                        Outcome::Registered
                    } else {
                        Outcome::Present
                    }
                }
                Err(error) if error.downcast_ref::<OperatorHeld>().is_some() => {
                    Outcome::Skipped("held (operator stop)")
                }
                Err(error) if error.downcast_ref::<AccountBusy>().is_some() => {
                    log::debug!(
                        "session_reconcile: {KEY}.{account}: busy; will register {} on a later pass",
                        binding.repository
                    );
                    Outcome::Deferred
                }
                Err(error) => {
                    let message = format!("{KEY}.{account} ({}): {error:#}", binding.repository);
                    mem.failures += 1;
                    mem.retry_at = now + backoff_secs(mem.failures);
                    if mem.last_error.as_deref() == Some(message.as_str()) {
                        log::debug!("session_reconcile: {message}");
                    } else {
                        log::warn!(
                            "session_reconcile: {message}; retrying in {}s",
                            backoff_secs(mem.failures)
                        );
                    }
                    mem.last_error = Some(message.clone());
                    Outcome::Failed { error: message }
                }
            }
        };
        pass.reports.push(Report {
            account: binding.account.clone(),
            repository: binding.repository.clone(),
            outcome,
        });
    }
    report_drift(declared, inputs, st, &mut pass);
    pass
}

/// Drift: repositories registered on an account that still has bindings, but
/// that none of them declares. Report only — a clone is never removed. An
/// account with an unparseable entry is skipped: the entry may name it.
fn report_drift(declared: &Declared, inputs: &Inputs<'_>, state: &mut State, pass: &mut Pass) {
    let mut accounts: Vec<&str> = Vec::new();
    for binding in &declared.bindings {
        if !accounts.contains(&binding.account.as_str()) {
            accounts.push(&binding.account);
        }
    }
    for account in accounts {
        if declared
            .invalid
            .iter()
            .any(|i| i.account.as_deref() == Some(account))
        {
            continue;
        }
        let Ok(Some(registered)) = (inputs.registered)(account) else {
            continue;
        };
        let undeclared: Vec<String> = registered
            .into_iter()
            .filter(|(_, key)| {
                !declared
                    .bindings
                    .iter()
                    .any(|b| b.account == account && &b.key == key)
            })
            .map(|(url, _)| url)
            .collect();
        if undeclared.is_empty() {
            state.drift_reported.remove(account);
            continue;
        }
        if state.drift_reported.get(account) != Some(&undeclared) {
            log::warn!(
                "session_reconcile: {KEY}.{account}: registered but no longer declared: {}; \
                 retained (nothing is deleted; remove it by hand once its work is recovered)",
                undeclared.join(", ")
            );
            state
                .drift_reported
                .insert(account.to_owned(), undeclared.clone());
        }
        pass.drift.push(Drift {
            account: account.to_owned(),
            undeclared,
        });
    }
}

#[cfg(test)]
#[path = "session_reconcile_bindings_tests.rs"]
mod tests;
