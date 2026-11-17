//! `autonomous.modelFallback` (#13873): the config contract, the rung walk,
//! and the fallback-aware pool preflight reads.
//!
//! Pool fixtures use synthetic per-class `.bad_tokens` marks exactly as the
//! Curator's dependency note describes (#13872 landed the class-scoped marks;
//! live marks may still be class-less — these tests pin both reads).

use super::*;
use crate::sweep_registry::test_support::write_config;
use serial_test::serial;
use std::fs;

/// A workspace whose per-repo pool holds `accounts` (one `*.token` file
/// each). Not inside a git worktree, so `resolve_tokens_dir` picks it.
fn pool_with(accounts: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(".loom").join("tokens");
    fs::create_dir_all(&dir).unwrap();
    for account in accounts {
        fs::write(dir.join(format!("{account}.token")), "sk-ant-oat01-fake").unwrap();
    }
    tmp
}

/// Mark `account` bad for the class `model` belongs to (#8058's own writer,
/// so the on-disk mark shape is exactly what production writes).
fn mark(ws: &Path, account: &str, model: &str) {
    crate::tokens_pool::bad_tokens::mark_bad_for_model(
        ws,
        account,
        "exhausted: hit your limit",
        Some(model),
    )
    .unwrap();
}

/// Every pool below uses two accounts, so "every account marked" is a
/// two-line fixture rather than a loop.
const ACCOUNTS: [&str; 2] = ["agent-1", "agent-2"];

// ---- config parsing ------------------------------------------------------

#[test]
#[serial(loom_config_env)]
fn absent_block_is_disabled_with_the_shipped_chain() {
    let tmp = pool_with(&ACCOUNTS);
    let config = read_model_fallback_config(tmp.path());
    assert!(!config.enabled);
    assert_eq!(
        config.chain,
        DEFAULT_CHAIN
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
#[serial(loom_config_env)]
fn parses_enabled_and_chain_dropping_blank_entries() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(
        tmp.path(),
        r#"{"autonomous": {"modelFallback": {"enabled": true, "chain": ["fable", " ", "opus"]}}}"#,
    );
    let config = read_model_fallback_config(tmp.path());
    assert!(config.enabled);
    assert_eq!(config.chain, vec!["fable".to_string(), "opus".to_string()]);
}

#[test]
#[serial(loom_config_env)]
fn malformed_values_soft_fail_to_the_disabled_default() {
    let tmp = pool_with(&ACCOUNTS);
    // A non-object block is ignored wholesale.
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": true}}"#);
    let config = read_model_fallback_config(tmp.path());
    assert!(!config.enabled);
    assert_eq!(config.chain.len(), 3);
    // A non-bool enabled soft-fails to false; a non-array chain keeps the
    // shipped one.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"modelFallback": {"enabled": "yes", "chain": "fable"}}}"#,
    );
    let config = read_model_fallback_config(tmp.path());
    assert!(!config.enabled);
    assert_eq!(config.chain.len(), 3);
    // An explicit empty array is honoured: enabled, but nowhere to fall
    // back to — the walk holds as today instead of inventing rungs.
    write_config(
        tmp.path(),
        r#"{"autonomous": {"modelFallback": {"enabled": true, "chain": []}}}"#,
    );
    let config = read_model_fallback_config(tmp.path());
    assert!(config.enabled);
    assert!(config.chain.is_empty());
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none(), "{effective:?}");
}

// ---- the rung walk -------------------------------------------------------

#[test]
#[serial(loom_config_env)]
fn falls_back_exactly_one_rung_when_the_configured_class_is_exhausted() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "opus");
    assert_eq!(effective.configured, "fable");
    let fallback = effective.fallback.unwrap();
    assert_eq!(fallback.from, "fable");
    assert_eq!(fallback.model, "opus");
    assert_eq!(fallback.reason, REASON_NO_ACCOUNT_FOR_CLASS);
}

#[test]
#[serial(loom_config_env)]
fn falls_to_sonnet_when_opus_is_exhausted_too() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
        mark(tmp.path(), account, "claude-opus-5");
    }
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "sonnet", "{effective:?}");
}

#[test]
#[serial(loom_config_env)]
fn holds_as_today_when_no_rung_is_usable() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        for model in ["fable", "claude-opus-5", "sonnet"] {
            mark(tmp.path(), account, model);
        }
    }
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none(), "{effective:?}");
}

#[test]
#[serial(loom_config_env)]
fn disabled_or_absent_reproduces_today_exactly() {
    let tmp = pool_with(&ACCOUNTS);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    // Absent block.
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none());
    // Explicitly disabled.
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": false}}}"#);
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none());
}

#[test]
#[serial(loom_config_env)]
fn a_reappearing_usable_account_ends_the_fallback_on_the_next_resolve() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    assert_eq!(effective_model(tmp.path(), "fable").model, "opus");
    // Nothing is cached: the marks aging out (or an operator unblocking one
    // account) flips the very next resolve back to the configured model.
    fs::remove_file(tmp.path().join(".loom").join("tokens").join(".bad_tokens")).unwrap();
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none(), "{effective:?}");
}

#[test]
#[serial(loom_config_env)]
fn never_climbs_above_the_configured_model() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    // The bottom rung has nothing below it in the chain.
    for account in ACCOUNTS {
        mark(tmp.path(), account, "sonnet");
    }
    let effective = effective_model(tmp.path(), "sonnet");
    assert_eq!(effective.model, "sonnet");
    assert!(effective.fallback.is_none(), "{effective:?}");
    // A configured model whose class is not in the chain at all has no
    // provable "below": haiku stays pinned even with every account marked.
    let effective = effective_model(tmp.path(), "haiku");
    assert_eq!(effective.model, "haiku");
    assert!(effective.fallback.is_none(), "{effective:?}");
}

#[test]
#[serial(loom_config_env)]
fn pinned_ids_match_the_chain_by_class() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "claude-fable-5");
    }
    let effective = effective_model(tmp.path(), "claude-fable-5");
    assert_eq!(effective.configured, "claude-fable-5");
    assert_eq!(effective.model, "opus");
}

#[test]
#[serial(loom_config_env)]
fn unrecognized_class_and_empty_model_never_fall_back() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    // An unrecognized model selects account-wide; every account IS marked,
    // but falling back would fail closed on a name the classifier does not
    // know, so it stays put.
    let effective = effective_model(tmp.path(), "gpt-5");
    assert_eq!(effective.model, "gpt-5");
    assert!(effective.fallback.is_none());
    // The CLI-default pass-through sentinel resolves to no model at all.
    let effective = effective_model(tmp.path(), "");
    assert_eq!(effective.model, "");
    assert!(effective.fallback.is_none());
}

#[test]
#[serial(loom_config_env)]
fn blank_and_unknown_chain_entries_are_skipped_not_fatal() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(
        tmp.path(),
        r#"{"autonomous": {"modelFallback": {
            "enabled": true,
            "chain": ["fable", "  ", "gpt-9", "opus"]
        }}}"#,
    );
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    // "gpt-9" classifies to nothing and cannot be probed; the walk skips it
    // and lands on the next recognized rung rather than failing the chain.
    let effective = effective_model(tmp.path(), "fable");
    assert_eq!(effective.model, "opus", "{effective:?}");
}

#[test]
#[serial(loom_config_env)]
fn no_pool_to_fall_back_within() {
    // Pin the shared pool to an empty dir too, or the host's real shared
    // pool would answer the probe (paths.rs's resolution order).
    let shared = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_SHARED_TOKENS_DIR", shared.path());
    let tmp = pool_with(&[]); // no *.token files: `total == 0`
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    let effective = effective_model(tmp.path(), "fable");
    std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none(), "{effective:?}");
}

#[test]
#[serial(loom_config_env)]
fn an_explicit_credential_spawn_never_changes_model() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    // spawn-claude.sh skips token selection entirely under this name, so
    // pool state must not change the model (the same two names its own
    // gate checks).
    std::env::set_var("LOOM_SPAWN_NO_EXPORT", "1");
    let effective = effective_model(tmp.path(), "fable");
    std::env::remove_var("LOOM_SPAWN_NO_EXPORT");
    assert_eq!(effective.model, "fable");
    assert!(effective.fallback.is_none(), "{effective:?}");
}

// ---- the fallback-aware preflight reads ----------------------------------

#[test]
#[serial(loom_config_env)]
fn launch_preflight_disabled_reads_account_wide() {
    let tmp = pool_with(&ACCOUNTS);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    let hold = exhausted_for_launch(tmp.path(), || Some("fable".to_string())).unwrap();
    assert_eq!(hold.pool.total, 2);
    assert_eq!(hold.pool.usable, 0);
    assert!(hold.class.is_none(), "disabled keeps the account-wide read");
}

#[test]
#[serial(loom_config_env)]
fn launch_preflight_enabled_lets_a_rung_serve_the_launch() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    assert!(
        exhausted_for_launch(tmp.path(), || Some("fable".to_string())).is_none(),
        "opus capacity exists: the launch must not be held"
    );
}

#[test]
#[serial(loom_config_env)]
fn launch_preflight_enabled_holds_only_when_every_rung_is_dead() {
    let tmp = pool_with(&ACCOUNTS);
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    for account in ACCOUNTS {
        for model in ["fable", "claude-opus-5", "sonnet"] {
            mark(tmp.path(), account, model);
        }
    }
    let hold = exhausted_for_launch(tmp.path(), || Some("fable".to_string())).unwrap();
    assert_eq!(hold.class.as_deref(), Some("fable"));
    assert_eq!(hold.pool.usable, 0);
}

#[test]
#[serial(loom_config_env)]
fn model_less_preflight_counts_class_scoped_marks_only_when_enabled() {
    // The preflights' exhausted predicate: `total > 0 && usable == 0` on the
    // [`preflight_pool_state`] read.
    fn exhausted(ws: &Path) -> bool {
        let pool = preflight_pool_state(ws);
        pool.total > 0 && pool.usable == 0
    }
    let tmp = pool_with(&ACCOUNTS);
    for account in ACCOUNTS {
        mark(tmp.path(), account, "fable");
    }
    // Disabled: byte-identical account-wide read — the marks hold the pool.
    assert!(exhausted(tmp.path()));
    let pool = preflight_pool_state(tmp.path());
    assert_eq!(pool.usable, 0);
    // Enabled: the any-class count sees the opus capacity the marks leave.
    write_config(tmp.path(), r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#);
    assert!(!exhausted(tmp.path()));
    let pool = preflight_pool_state(tmp.path());
    assert_eq!(pool.usable, 2);
    // ...but a class-LESS mark still holds everything, enabled or not.
    crate::tokens_pool::bad_tokens::mark_bad(tmp.path(), "agent-1", "exhausted: weekly limit")
        .unwrap();
    crate::tokens_pool::bad_tokens::mark_bad(tmp.path(), "agent-2", "exhausted: weekly limit")
        .unwrap();
    assert!(exhausted(tmp.path()));
}
