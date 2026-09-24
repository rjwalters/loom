//! End-to-end tests for the API-key-pool credential attribution the terminal
//! journals carry (Issue #8447).
//!
//! In their own sibling module rather than appended to `tests`: that module is
//! already near the file-size ratchet's threshold
//! (`scripts/check-file-size-budget.sh`), and the rule there is to add a new
//! sibling rather than grow a big one — the same reason `timeline_tests` exists.

use super::*;
use crate::sweep_registry::test_support::*;
use tempfile::tempdir;

/// The dispatch header + `# LOOM_LAUNCH` record a pool-sourced native spawn
/// leaves in its own per-sweep log, verbatim in shape (see
/// `worker_spawn::run`).
fn pool_sourced_log(sweep_id: &str, issue: u32, provider: &str, account: &str) -> String {
    format!(
        "==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\n\
         spawn-worker: runtime=pi (from config (runtimes.default))\n\
         # LOOM_LAUNCH {}\n\
         # LOOM_CLI_START runtime=pi\n",
        serde_json::json!({
            "schema": 1,
            "runtime": "pi",
            "provider": "zai-coding-plan",
            "model": "glm-5.3",
            "profile": "zai-flash",
            "effort": serde_json::Value::Null,
            "credentialSource": "pool",
            "credentialProvider": provider,
            "credentialAccount": account,
            "usage": "native-json-events",
            "billing": "not-measured",
        })
    )
}

/// The same shape for a spawn whose credential came from the launching
/// environment: a source, and deliberately no account.
fn env_sourced_log(sweep_id: &str, issue: u32) -> String {
    format!(
        "==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\n\
         # LOOM_LAUNCH {}\n",
        serde_json::json!({
            "schema": 1,
            "runtime": "pi",
            "credentialSource": "env",
            "credentialProvider": serde_json::Value::Null,
            "credentialAccount": serde_json::Value::Null,
        })
    )
}

/// AC1: a pool-sourced native spawn's journal entry carries the credential
/// source, the pool provider namespace, and the account name.
#[test]
fn journal_entry_carries_the_pool_provider_and_account_name() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8447-0";
    insert_dead_running_with_log(
        &mut registry,
        8447,
        0,
        "unknown",
        &pool_sourced_log(sweep_id, 8447, "zai", "alpha"),
    );
    registry.reap_once();

    let path = registry.config().resolve_outcomes_journal_path();
    let records = sweep_outcomes::read_all(&path);
    let record = records
        .iter()
        .find(|r| r.issue == 8447)
        .expect("terminal outcome must be journaled");
    let credential = record
        .credential
        .as_ref()
        .expect("a pool-sourced spawn's journal entry must carry its credential attribution");
    assert_eq!(credential.source, "pool");
    assert_eq!(credential.provider.as_deref(), Some("zai"));
    assert_eq!(credential.account.as_deref(), Some("alpha"));
}

/// AC1 (second half): an env-sourced spawn records its source with no account.
#[test]
fn an_env_sourced_spawn_records_its_source_with_no_account() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8448-0";
    insert_dead_running_with_log(
        &mut registry,
        8448,
        0,
        "unknown",
        &env_sourced_log(sweep_id, 8448),
    );
    registry.reap_once();

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let credential = records
        .iter()
        .find(|r| r.issue == 8448)
        .expect("journaled")
        .credential
        .clone()
        .expect("an env-sourced spawn still records WHERE its credential came from");
    assert_eq!(credential.source, "env");
    assert_eq!(credential.provider, None);
    assert_eq!(credential.account, None);
}

/// A Claude / legacy-adapter spawn writes no launch record at all. The field
/// is then absent — never a fabricated account, and never a `"unknown"`
/// placeholder that would be indistinguishable from a real pool spawn whose
/// account could not be recovered.
#[test]
fn a_spawn_with_no_launch_record_journals_no_credential_attribution() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    insert_dead_running_with_log(
        &mut registry,
        8449,
        0,
        "agent-2",
        "==== loom-daemon dispatch: sweep_id=sweep-issue-8449-0 issue=8449 ====\nclean run\n",
    );
    registry.reap_once();

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let record = records.iter().find(|r| r.issue == 8449).expect("journaled");
    assert_eq!(record.credential, None);
    // The OAuth pool's own attribution is untouched by this addition.
    assert_eq!(record.token_name, "agent-2");
}

/// AC1's explicit safety requirement: no key material is present in the
/// journal. The launch record itself carries names only, so this asserts the
/// property end-to-end over the serialized journal bytes — including a log
/// that (wrongly) also held a secret-looking line, which must not be scraped
/// into the record by some future widening of the parser.
#[test]
fn no_key_material_reaches_the_journal() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = "sweep-issue-8450-0";
    let log = format!(
        "{}ZAI_API_KEY=sk-fake-key-material-8447\n",
        pool_sourced_log(sweep_id, 8450, "zai", "alpha")
    );
    insert_dead_running_with_log(&mut registry, 8450, 0, "unknown", &log);
    registry.reap_once();

    let journal = registry.config().resolve_outcomes_journal_path();
    let raw = std::fs::read_to_string(&journal).expect("journal written");
    assert!(raw.contains("\"account\":\"alpha\""), "attribution must be present: {raw}");
    assert!(!raw.contains("sk-fake-key-material-8447"), "key material in the journal: {raw}");
    assert!(
        !raw.contains("ZAI_API_KEY"),
        "even the variable's value context must not leak: {raw}"
    );

    // Same property on the paired `sweep.outcome` telemetry journal, whose
    // `config` map carries the same three names (#8447).
    let telemetry = registry.config().resolve_outcome_telemetry_path();
    let raw = std::fs::read_to_string(&telemetry).expect("telemetry journal written");
    assert!(raw.contains("credential_account"), "telemetry must carry attribution: {raw}");
    assert!(raw.contains("zai"), "telemetry must name the provider: {raw}");
    assert!(!raw.contains("sk-fake-key-material-8447"), "key material in telemetry: {raw}");
}

/// A per-issue log is reused across dispatches, so an earlier run's launch
/// record must never be attributed to this one.
#[test]
fn a_previous_dispatchs_launch_record_is_not_attributed_to_this_sweep() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let log = format!(
        "{}{}",
        pool_sourced_log("sweep-issue-8451-0", 8451, "zai", "stale"),
        pool_sourced_log("sweep-issue-8451-1", 8451, "zai", "fresh"),
    );
    insert_dead_running_with_log(&mut registry, 8451, 1, "unknown", &log);
    registry.reap_once();

    let records = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let credential = records
        .iter()
        .find(|r| r.issue == 8451)
        .expect("journaled")
        .credential
        .clone()
        .expect("attribution present");
    assert_eq!(credential.account.as_deref(), Some("fresh"));
}

/// A journal line written before this field existed must still parse —
/// `read_all` drops any line it cannot deserialize, so a missing default
/// would silently erase every pre-#8447 record.
#[test]
fn a_pre_8447_journal_line_without_the_field_still_parses() {
    let line = serde_json::json!({
        "timestamp": "2026-09-01T00:00:00Z",
        "repo": "/tmp/repo",
        "issue": 1,
        "sweep_id": "sweep-issue-1-0",
        "outcome": "exited",
        "token_name": "agent-1",
        "duration_sec": 42,
    })
    .to_string();
    let dir = tempdir().unwrap();
    let path = dir.path().join("sweep-outcomes.jsonl");
    std::fs::write(&path, format!("{line}\n")).unwrap();
    let records = sweep_outcomes::read_all(&path);
    assert_eq!(records.len(), 1, "a pre-#8447 line must survive");
    assert_eq!(records[0].credential, None);
}
