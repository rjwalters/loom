//! Tests for the Kimi Code CLI session-store reader (Issue #8564).
//!
//! # Fixture provenance — read this before trusting the fixture
//!
//! The fixture below is **reconstructed from the shipped
//! `@moonshot-ai/kimi-code@2.0.2` bundle**, not captured from a live Kimi
//! session. That distinction is load-bearing and is not hedging: no Kimi
//! subscription credential and no Moonshot API key exist in the environment
//! this reader was built in (the same gap
//! `docs/experiments/kimi-harness-probe-2026-09-22.json` records for #8561's
//! canary, tracked in #8606), so fabricating a "captured" transcript would
//! have been fabricating evidence.
//!
//! What the fixture IS grounded in, line by line, is the bundle's own
//! serializers read on 2026-09-22 — `Event2.serialize()` (flat
//! `{"type", …payload, "time"}` envelope), `usageRecordSchema`
//! (`{agentId, model, usage, usageScope}`), `emptyUsage()`/`addUsage`
//! (`{inputOther, output, inputCacheRead, inputCacheCreation}`),
//! `llmRequestSchema` (`{provider, model, modelAlias, systemPrompt?, …}`) and
//! `appendSessionIndexEntry` (`{sessionId, sessionDir, workDir}` /
//! `{sessionId, deleted}`). Every field name and nesting level here was copied
//! from those definitions rather than invented.
//!
//! Consequence for the acceptance criteria: the *shape* is verified, the
//! *values* are synthetic. A live receipt (a real Kimi sweep whose completion
//! carries `runtime: kimi` + provider + model) still has to be taken once a
//! credential exists — that is #8606's job, not something these tests can
//! stand in for.

use super::*;
use std::fs;

/// Redacted stand-ins for the prompt/tool-output text a real `wire.jsonl`
/// interleaves between the two record types this reader decodes. Planted so
/// `a_wire_log_full_of_secrets_yields_only_counters` can prove the reader
/// never carries any of it out, rather than merely asserting on what it
/// happened to return.
const PLANTED_SECRET: &str = "sk-live-PLANTED-SECRET-DO-NOT-SURFACE";

fn usage_line(alias: &str, time_ms: i64, usage: (i64, i64, i64, i64)) -> String {
    let (input_other, output, cache_read, cache_creation) = usage;
    serde_json::json!({
        "type": USAGE_RECORD_TYPE,
        "agentId": "main",
        "model": alias,
        "usage": {
            "inputOther": input_other,
            "output": output,
            "inputCacheRead": cache_read,
            "inputCacheCreation": cache_creation,
        },
        "usageScope": "turn",
        "time": time_ms,
    })
    .to_string()
}

fn llm_request_line(alias: &str, model: &str, provider: &str, time_ms: i64) -> String {
    serde_json::json!({
        "type": LLM_REQUEST_TYPE,
        "agentId": "main",
        "kind": "loop",
        "provider": provider,
        "model": model,
        "modelAlias": alias,
        // The real record carries the system prompt whenever it differs from
        // the bound profile's. Planted here on purpose.
        "systemPrompt": format!("You are a coding agent. Credential: {PLANTED_SECRET}"),
        "toolSelect": true,
        "systemPromptHash": "deadbeef",
        "toolsHash": "cafebabe",
        "messageCount": 7,
        "time": time_ms,
    })
    .to_string()
}

/// Conversation records this reader must skip untouched — one user prompt and
/// one tool result, both carrying planted secrets, in the flat envelope the
/// bundle's `Event2.serialize()` produces.
fn noise_lines(time_ms: i64) -> Vec<String> {
    vec![
        serde_json::json!({
            "type": "context.append_message",
            "agentId": "main",
            "message": {"role": "user", "content": [{"type": "text", "text": PLANTED_SECRET}]},
            "time": time_ms,
        })
        .to_string(),
        serde_json::json!({
            "type": "tool.result",
            "agentId": "main",
            "output": format!("cat ~/.netrc -> {PLANTED_SECRET}"),
            "time": time_ms,
        })
        .to_string(),
        "# not a JSON line at all".to_string(),
        "{ not json".to_string(),
    ]
}

/// One session's worth of on-disk state, laid out exactly as the CLI does:
/// `<root>/sessions/<workDirKey>/<sessionId>/agents/<agentId>/wire.jsonl`.
struct Session<'a> {
    id: &'a str,
    work_dir: &'a str,
    /// `(agentId, wire.jsonl lines)`.
    agents: Vec<(&'a str, Vec<String>)>,
}

/// Write a whole fixture data root: `session_index.jsonl` plus each session's
/// directory tree. Returns the root.
fn seed_root(root: &Path, sessions: &[Session<'_>], tombstones: &[&str]) {
    fs::create_dir_all(root).unwrap();
    let mut index = String::new();
    for session in sessions {
        let session_dir = root
            .join("sessions")
            .join(session.work_dir.replace('/', "-").trim_start_matches('-'))
            .join(session.id);
        for (agent, lines) in &session.agents {
            let agent_dir = session_dir.join("agents").join(agent);
            fs::create_dir_all(&agent_dir).unwrap();
            let mut body = String::from(
                "{\"type\":\"metadata\",\"protocol_version\":\"1.5\",\"created_at\":0}\n",
            );
            for line in lines {
                body.push_str(line);
                body.push('\n');
            }
            fs::write(agent_dir.join(WIRE_LOG), body).unwrap();
        }
        index.push_str(
            &serde_json::json!({
                "sessionId": session.id,
                "sessionDir": session_dir.to_str().unwrap(),
                "workDir": session.work_dir,
            })
            .to_string(),
        );
        index.push('\n');
    }
    for id in tombstones {
        index.push_str(&serde_json::json!({"sessionId": id, "deleted": true}).to_string());
        index.push('\n');
    }
    fs::write(root.join(SESSION_INDEX), index).unwrap();
}

/// The canonical fixture: one session in `/w/loom`, one `llm.request` binding
/// the `code` alias to `kimi-k2.7-code` on the `kimi` provider, and two
/// `usage.record`s.
fn canonical(root: &Path) {
    seed_root(
        root,
        &[Session {
            id: "sess-aaa",
            work_dir: "/w/loom",
            agents: vec![("main", {
                let mut lines = vec![llm_request_line("code", "kimi-k2.7-code", "kimi", 1_000)];
                lines.extend(noise_lines(1_100));
                lines.push(usage_line("code", 2_000, (1_200, 340, 8_000, 500)));
                lines.push(usage_line("code", 3_000, (300, 60, 2_000, 0)));
                lines
            })],
        }],
        &[],
    );
}

fn dirs(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

// ---------------------------------------------------------------------------
// The counters
// ---------------------------------------------------------------------------

#[test]
fn a_session_fixture_folds_into_the_expected_totals_with_model_and_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    canonical(&root);

    let records = usage_records(&dirs(&["/w/loom"]), None, None, Some(tmp.path()));
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].model, "kimi-k2.7-code", "alias resolved via llm.request");
    assert_eq!(records[0].alias, "code");
    assert_eq!(records[0].provider.as_deref(), Some("kimi"));

    let totals = fold_records(records).expect("a fixture with counters must fold");
    assert_eq!(totals.len(), 1, "{totals:?}");
    let t = &totals[0];
    assert_eq!(t.model, "kimi-k2.7-code");
    assert_eq!(t.speed, "standard");
    assert_eq!(t.service_tier, "standard");
    assert_eq!(t.input, 1_500, "inputOther only — cache columns are separate");
    assert_eq!(t.output, 400);
    assert_eq!(t.cache_read, 10_000);
    assert_eq!(
        t.cache_write_5m, 500,
        "Kimi's published default TTL is 5min; the CLI sends none"
    );
    assert_eq!(t.cache_write_1h, 0, "never the 1h bucket — see the module doc");
}

#[test]
fn a_fixture_without_counters_yields_no_totals_rather_than_zeros() {
    // "Unknown is not zero": Kimi writes a `usage.record` even for a request
    // that billed nothing (`usage ?? emptyUsage()` at the call site), so an
    // all-zero record must NOT publish a 0-token badge for a model that was
    // never charged.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[Session {
            id: "sess-zero",
            work_dir: "/w/loom",
            agents: vec![(
                "main",
                vec![
                    llm_request_line("code", "kimi-k2.7-code", "kimi", 1_000),
                    usage_line("code", 2_000, (0, 0, 0, 0)),
                ],
            )],
        }],
        &[],
    );
    assert!(
        tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).is_none(),
        "an all-zero usage.record must yield None, never Some(vec![]) or a zeroed row"
    );
}

#[test]
fn a_wire_log_with_no_usage_records_at_all_yields_none() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[Session {
            id: "sess-quiet",
            work_dir: "/w/loom",
            agents: vec![("main", noise_lines(1_000))],
        }],
        &[],
    );
    assert!(tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).is_none());
}

#[test]
fn an_absent_data_root_is_none_not_an_empty_breakdown() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(discover_kimi_homes(Some(tmp.path())).is_empty());
    assert!(tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).is_none());
}

// ---------------------------------------------------------------------------
// Model / provider resolution
// ---------------------------------------------------------------------------

#[test]
fn an_alias_with_no_llm_request_keeps_the_alias_never_a_guessed_model_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[Session {
            id: "sess-bare",
            work_dir: "/w/loom",
            agents: vec![("main", vec![usage_line("my-alias", 2_000, (10, 5, 0, 0))])],
        }],
        &[],
    );
    let records = usage_records(&dirs(&["/w/loom"]), None, None, Some(tmp.path()));
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].model, "my-alias",
        "the alias was read off disk; anything else would be invented"
    );
    assert_eq!(records[0].provider, None, "no provider was recorded — not a guess");
}

#[test]
fn every_agents_wire_log_is_read_not_only_the_main_one() {
    // A subagent bills its own tokens into its own agents/<id>/wire.jsonl.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[Session {
            id: "sess-swarm",
            work_dir: "/w/loom",
            agents: vec![
                (
                    "main",
                    vec![
                        llm_request_line("code", "kimi-k2.7-code", "kimi", 900),
                        usage_line("code", 1_000, (100, 10, 0, 0)),
                    ],
                ),
                (
                    "sub-1",
                    vec![
                        llm_request_line("fast", "kimi-k3-fast", "kimi", 1_100),
                        usage_line("fast", 1_200, (50, 5, 0, 0)),
                    ],
                ),
            ],
        }],
        &[],
    );
    let totals = tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).unwrap();
    let models: Vec<&str> = totals.iter().map(|t| t.model.as_str()).collect();
    assert_eq!(models, vec!["kimi-k2.7-code", "kimi-k3-fast"], "{totals:?}");
}

// ---------------------------------------------------------------------------
// Attribution: session id, directory, window
// ---------------------------------------------------------------------------

#[test]
fn a_sibling_sessions_work_dir_is_not_folded_in() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[
            Session {
                id: "sess-mine",
                work_dir: "/w/loom",
                agents: vec![(
                    "main",
                    vec![
                        llm_request_line("code", "kimi-k2.7-code", "kimi", 900),
                        usage_line("code", 1_000, (100, 10, 0, 0)),
                    ],
                )],
            },
            Session {
                id: "sess-theirs",
                work_dir: "/w/loom/.loom/worktrees/issue-9999",
                agents: vec![(
                    "main",
                    vec![
                        llm_request_line("code", "kimi-k2.7-code", "kimi", 900),
                        usage_line("code", 1_000, (999_999, 999_999, 0, 0)),
                    ],
                )],
            },
        ],
        &[],
    );
    let totals = tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).unwrap();
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].input, 100, "a concurrent sibling's tokens must not be folded in");
}

#[test]
fn an_exact_session_id_outranks_the_directory_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[Session {
            id: "sess-resumed",
            // A resumed session can legitimately carry a workDir outside the
            // caller's set; the id is the precise key and must still win.
            work_dir: "/somewhere/else",
            agents: vec![(
                "main",
                vec![
                    llm_request_line("code", "kimi-k2.7-code", "kimi", 900),
                    usage_line("code", 1_000, (7, 3, 0, 0)),
                ],
            )],
        }],
        &[],
    );
    assert!(
        tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).is_none(),
        "directory filter alone must not match"
    );
    let totals =
        tokens_by_model(&dirs(&["/w/loom"]), Some("sess-resumed"), None, Some(tmp.path())).unwrap();
    assert_eq!(totals[0].input, 7);
}

#[test]
fn the_window_filters_on_each_records_own_time_not_the_sessions() {
    // One long-lived Kimi session can span several launches, so the per-record
    // `time` is the only key that separates them.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    canonical(&root);
    let at = |ms: i64| DateTime::<Utc>::from_timestamp_millis(ms).unwrap();

    let totals =
        tokens_by_model(&dirs(&["/w/loom"]), None, Some((at(2_500), at(3_500))), Some(tmp.path()))
            .unwrap();
    assert_eq!(totals[0].input, 300, "only the second record is inside the window");
    assert_eq!(totals[0].output, 60);

    assert!(
        tokens_by_model(
            &dirs(&["/w/loom"]),
            None,
            Some((at(10_000), at(20_000))),
            Some(tmp.path())
        )
        .is_none(),
        "a window with nothing in it is unmeasured, not zero"
    );
}

#[test]
fn a_deleted_session_is_dropped_by_its_tombstone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    seed_root(
        &root,
        &[Session {
            id: "sess-gone",
            work_dir: "/w/loom",
            agents: vec![(
                "main",
                vec![
                    llm_request_line("code", "kimi-k2.7-code", "kimi", 900),
                    usage_line("code", 1_000, (100, 10, 0, 0)),
                ],
            )],
        }],
        &["sess-gone"],
    );
    assert!(session_index(&root).is_empty(), "a tombstoned id must not survive the index");
    assert!(tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).is_none());
}

#[test]
fn a_garbage_index_line_is_skipped_not_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    canonical(&root);
    let index = root.join(SESSION_INDEX);
    let existing = fs::read_to_string(&index).unwrap();
    fs::write(&index, format!("not json\n{{\"sessionId\":\"no-dirs\"}}\n{existing}")).unwrap();
    assert_eq!(session_index(&root).len(), 1);
    assert!(tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).is_some());
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

#[test]
fn a_wire_log_full_of_secrets_yields_only_counters() {
    // `wire.jsonl` holds the whole conversation. The reader decodes exactly
    // two record types and, from them, only the model/provider strings and
    // four integers — so no planted secret can reach a caller through any
    // field of the returned records.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    canonical(&root);

    let records = usage_records(&dirs(&["/w/loom"]), None, None, Some(tmp.path()));
    let rendered = format!("{records:?}");
    assert!(
        !rendered.contains(PLANTED_SECRET),
        "the reader carried conversation/system-prompt text out: {rendered}"
    );
    assert!(!rendered.contains("netrc"), "{rendered}");
    let totals = format!("{:?}", fold_records(records).unwrap());
    assert!(!totals.contains(PLANTED_SECRET), "{totals}");
}

#[test]
fn an_over_long_record_is_skipped_rather_than_read_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(DEFAULT_HOME_DIR);
    canonical(&root);
    // Append one pathological record AFTER the good ones: the good ones must
    // still be returned, and the reader must not choke on the giant line.
    let log = root
        .join("sessions")
        .join("w-loom")
        .join("sess-aaa")
        .join("agents")
        .join("main")
        .join(WIRE_LOG);
    let giant = serde_json::json!({
        "type": "tool.result",
        "output": "x".repeat((MAX_LINE_BYTES as usize) + 1_024),
        "time": 4_000,
    })
    .to_string();
    let mut body = fs::read_to_string(&log).unwrap();
    body.push_str(&giant);
    body.push('\n');
    body.push_str(&usage_line("code", 5_000, (1, 1, 0, 0)));
    body.push('\n');
    fs::write(&log, body).unwrap();

    let totals = tokens_by_model(&dirs(&["/w/loom"]), None, None, Some(tmp.path())).unwrap();
    assert_eq!(totals.len(), 1, "{totals:?}");
    assert!(totals[0].input >= 1_500, "the pre-existing records survive: {totals:?}");
}

// ---------------------------------------------------------------------------
// Data-root discovery
// ---------------------------------------------------------------------------

/// A named `home` is `<home>/.kimi-code`, and — deliberately — does NOT
/// consult either environment variable, so this needs no `#[serial]` guard and
/// cannot be perturbed by the env-mutating test below running beside it.
#[test]
fn the_default_root_is_dot_kimi_code_under_a_named_home() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(discover_kimi_homes(Some(tmp.path())).is_empty(), "absent root -> no candidates");
    fs::create_dir_all(tmp.path().join(DEFAULT_HOME_DIR)).unwrap();
    assert_eq!(discover_kimi_homes(Some(tmp.path())), vec![tmp.path().join(DEFAULT_HOME_DIR)]);
}

/// The env ladder applies only to the production call shape (`home: None`).
/// `LOOM_KIMI_CODE_HOME` outranks the CLI's own `KIMI_CODE_HOME`, and each
/// names the data root **itself**, not a home directory to append
/// `.kimi-code` to — matching the CLI's `defaultHomeDir`.
#[test]
#[serial_test::serial(kimi_home_env)]
fn the_loom_override_outranks_the_clis_own_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let loom_root = tmp.path().join("loom-pinned");
    let cli_root = tmp.path().join("cli-pinned");
    fs::create_dir_all(&loom_root).unwrap();
    fs::create_dir_all(&cli_root).unwrap();

    std::env::set_var(KIMI_CODE_HOME_ENV, &cli_root);
    assert_eq!(discover_kimi_homes(None), vec![cli_root.clone()]);
    std::env::set_var(KIMI_HOME_ENV, &loom_root);
    assert_eq!(discover_kimi_homes(None), vec![loom_root]);

    // An override naming a path that does not exist is "no data root", never a
    // silent fall-through to the real `~/.kimi-code` of whoever is running the
    // suite — that fall-through would read a developer's own sessions.
    std::env::set_var(KIMI_HOME_ENV, tmp.path().join("does-not-exist"));
    assert!(discover_kimi_homes(None).is_empty());

    std::env::remove_var(KIMI_HOME_ENV);
    std::env::remove_var(KIMI_CODE_HOME_ENV);
}
