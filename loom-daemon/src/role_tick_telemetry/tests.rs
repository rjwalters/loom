//! Unit tests for the `role_tick.outcome` record (Issue #8056).

use super::*;
use chrono::TimeZone as _;
use std::fs;
use tempfile::tempdir;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
}

fn tick(result: RoleTickResult) -> RoleTickTelemetry {
    RoleTickTelemetry {
        root: PathBuf::from("/repo/a"),
        role: "judge".to_string(),
        started_at: at(0),
        ended_at: at(42),
        result,
        model: Some("claude-sonnet-5".to_string()),
        effort: Some("high".to_string()),
        detail: None,
        gated_pool: None,
    }
}

fn totals(model: &str, input: i64, output: i64) -> ModelUsageTotals {
    ModelUsageTotals {
        model: model.to_string(),
        speed: "standard".to_string(),
        service_tier: "standard".to_string(),
        input,
        output,
        ..ModelUsageTotals::default()
    }
}

/// A transcript head naming a `/loom:<role>` slash command, in the shape
/// Claude Code actually writes (JSON-escaped markup inside the first `user`
/// record).
fn role_head(role: &str) -> String {
    format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\
         \"<command-message>loom:{role}</command-message>\\n\
         <command-name>/loom:{role}</command-name>\\n\
         <command-args></command-args>\"}}}}\n"
    )
}

fn usage_line(model: &str, input: i64, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"message\":{{\"model\":\"{model}\",\
         \"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output}}}}}}}\n"
    )
}

fn bash_line(command: &str) -> String {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\
         \"name\":\"Bash\",\"input\":{{\"command\":\"{escaped}\"}}}}]}}}}\n"
    )
}

// ------------------------------------------------------------------------
// Attribution
// ------------------------------------------------------------------------

#[test]
fn head_names_role_matches_only_the_exact_command() {
    assert!(head_names_role(&role_head("judge"), "judge"));
    assert!(head_names_role(&role_head("JUDGE"), "judge"));
    // A different role's session in the same project directory.
    assert!(!head_names_role(&role_head("curator"), "judge"));
    // A prefix must not match — `/loom:sweep` is not `/loom:sw`.
    assert!(!head_names_role(&role_head("judgement"), "judge"));
    // No slash command at all (a plain interactive session).
    assert!(!head_names_role("just some prose about the judge", "judge"));
}

#[test]
fn attributed_transcripts_filters_by_role_and_window() {
    let dir = tempdir().unwrap();
    let projects = dir.path().join("projects");
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    let project = projects.join(crate::transcript_tokens::project_slug(&root));
    fs::create_dir_all(&project).unwrap();

    let mine = project.join("aaa.jsonl");
    fs::write(&mine, role_head("judge")).unwrap();
    let other_role = project.join("bbb.jsonl");
    fs::write(&other_role, role_head("curator")).unwrap();

    // The window is anchored on "now" because the fixtures' mtimes are now.
    let now = Utc::now();
    let found = attributed_transcripts(&projects, &root, "judge", now, now);
    assert_eq!(found, vec![mine.clone()]);

    // A window far in the past excludes everything, even the right role —
    // the mtime half of the key is load-bearing.
    let stale = attributed_transcripts(&projects, &root, "judge", at(0), at(1));
    assert!(stale.is_empty(), "out-of-window transcript must not attribute");
}

#[test]
fn mtime_in_tick_window_is_fail_closed_for_an_unreadable_file() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("nope.jsonl");
    assert!(
        !mtime_in_tick_window(&missing, at(0), at(10)),
        "an unstattable file must be rejected, not folded in on a guess"
    );
}

// ------------------------------------------------------------------------
// Action tallying
// ------------------------------------------------------------------------

#[test]
fn tally_command_classifies_each_forge_action() {
    let mut a = RoleTickActions::default();
    tally_command("gh issue edit 42 --add-label \"loom:building\"", &mut a);
    assert_eq!(a.issues_labeled, 1);
    tally_command("gh pr edit 7 --remove-label loom:review-requested", &mut a);
    assert_eq!(a.issues_labeled, 2);

    tally_command("./.loom/scripts/merge-pr.sh 99", &mut a);
    tally_command("gh pr merge 100 --squash", &mut a);
    assert_eq!(a.prs_merged, 2);

    tally_command("gh issue comment 42 --body-file /tmp/x", &mut a);
    tally_command("gh pr comment 7 --body hi", &mut a);
    tally_command("gh api repos/o/r/issues/1/comments -f body=hi", &mut a);
    assert_eq!(a.comments_posted, 3);
}

#[test]
fn tally_command_ignores_read_only_commands() {
    let mut a = RoleTickActions::default();
    tally_command("gh issue list --label loom:issue", &mut a);
    tally_command("gh pr view 7 --json labels", &mut a);
    tally_command("git status", &mut a);
    // `gh issue edit` WITHOUT a label flag is a body/title edit, not a label
    // transition — the axis this counter is named for.
    tally_command("gh issue edit 42 --body-file /tmp/b", &mut a);
    assert_eq!(a, RoleTickActions::default());
}

#[test]
fn tally_command_counts_a_compound_command_once_per_bucket() {
    let mut a = RoleTickActions::default();
    tally_command("gh issue edit 1 --add-label x && gh issue edit 2 --add-label y", &mut a);
    assert_eq!(
        a.issues_labeled, 1,
        "a chained command is a documented lower bound, never an over-count"
    );
}

// ------------------------------------------------------------------------
// Single-pass transcript scan
// ------------------------------------------------------------------------

#[test]
fn scan_transcripts_folds_tokens_and_actions_in_one_pass() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let mut text = role_head("judge");
    text.push_str(&usage_line("claude-sonnet-5", 100, 10));
    text.push_str(&bash_line("gh issue edit 42 --add-label loom:pr"));
    text.push_str(&usage_line("claude-opus-5", 5, 1));
    text.push_str(&bash_line("gh pr comment 9 --body ok"));
    fs::write(&path, text).unwrap();

    let scan = scan_transcripts(&[path]).expect("a readable transcript yields a scan");
    assert_eq!(scan.actions.issues_labeled, 1);
    assert_eq!(scan.actions.comments_posted, 1);
    assert_eq!(scan.actions.prs_merged, 0);
    let models: Vec<&str> = scan
        .tokens_by_model
        .iter()
        .map(|r| r.model.as_str())
        .collect();
    assert_eq!(models, vec!["claude-opus-5", "claude-sonnet-5"]);
    let sonnet = scan
        .tokens_by_model
        .iter()
        .find(|r| r.model == "claude-sonnet-5")
        .unwrap();
    assert_eq!((sonnet.input, sonnet.output), (100, 10));
}

#[test]
fn scan_transcripts_returns_none_when_nothing_is_readable() {
    assert_eq!(scan_transcripts(&[]), None);
    let dir = tempdir().unwrap();
    assert_eq!(scan_transcripts(&[dir.path().join("absent.jsonl")]), None);
}

#[test]
fn scan_transcripts_distinguishes_observed_zero_from_unobserved() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("quiet.jsonl");
    // A real session that did nothing forge-mutating and reported no usage.
    fs::write(&path, role_head("guide")).unwrap();
    let scan = scan_transcripts(&[path]).expect("a readable transcript is an observation");
    assert_eq!(scan.actions, RoleTickActions::default());
    assert!(scan.tokens_by_model.is_empty());
}

// ------------------------------------------------------------------------
// Record construction — the "unknown != zero" contract
// ------------------------------------------------------------------------

#[test]
fn build_record_carries_every_observed_field() {
    let scan = TranscriptScan {
        tokens_by_model: vec![
            totals("claude-opus-5", 1, 2),
            totals("claude-sonnet-5", 3, 4),
        ],
        actions: RoleTickActions {
            issues_labeled: 2,
            prs_merged: 1,
            comments_posted: 3,
        },
    };
    let record = build_record(
        &tick(RoleTickResult::Success),
        "rjwalters/loom".to_string(),
        RepoVisibility::Public,
        Some(scan),
    );
    assert_eq!(record.repo, "rjwalters/loom");
    assert_eq!(record.visibility, RepoVisibility::Public);
    assert_eq!(record.role, "judge");
    assert_eq!(record.duration_sec, 42);
    assert_eq!(record.result, RoleTickResult::Success);
    assert_eq!(record.model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(record.effort.as_deref(), Some("high"));
    assert_eq!(
        record.models_used,
        Some(vec!["claude-opus-5".to_string(), "claude-sonnet-5".to_string()])
    );
    assert_eq!(record.actions.unwrap().prs_merged, 1);
    assert_eq!(record.tokens_by_model.unwrap().len(), 2);
}

#[test]
fn build_record_omits_rather_than_zeroes_an_unobserved_scan() {
    let record = build_record(
        &tick(RoleTickResult::Success),
        "rjwalters/loom".to_string(),
        RepoVisibility::Private,
        None,
    );
    assert_eq!(record.tokens_by_model, None);
    assert_eq!(record.models_used, None);
    assert_eq!(record.actions, None);

    let json = serde_json::to_value(&record).unwrap();
    for key in ["tokens_by_model", "models_used", "actions"] {
        assert!(
            json.get(key).is_none(),
            "{key} must be an ABSENT key, not a null/zero — a consumer has to be \
             able to tell 'not observed' from 'observed, none'"
        );
    }
}

#[test]
fn build_record_keeps_an_observed_zero_action_count() {
    let record = build_record(
        &tick(RoleTickResult::Success),
        "rjwalters/loom".to_string(),
        RepoVisibility::Private,
        Some(TranscriptScan::default()),
    );
    // Observed-and-empty: the actions object is present with zeros, and the
    // token list — genuinely empty — is still omitted (it follows
    // `tokens_by_model`'s own never-an-empty-vec contract).
    assert_eq!(record.actions, Some(RoleTickActions::default()));
    assert_eq!(record.tokens_by_model, None);
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["actions"]["issues_labeled"], 0);
}

/// Issue #8056 test plan: a tick that skipped before spawning must report the
/// right `result` and must NOT fabricate token counts — even if a stale
/// transcript for the same role happens to sit inside the slack window.
#[test]
fn build_record_never_fabricates_tokens_for_a_pre_spawn_skip() {
    let poisoned = TranscriptScan {
        tokens_by_model: vec![totals("claude-sonnet-5", 999, 999)],
        actions: RoleTickActions {
            issues_labeled: 9,
            prs_merged: 9,
            comments_posted: 9,
        },
    };
    for result in [
        RoleTickResult::SkippedNoTokenPool,
        RoleTickResult::SkippedPoolExhausted,
        RoleTickResult::SkippedModelRuntimeMismatch,
        RoleTickResult::RuntimeRejected,
    ] {
        let record = build_record(
            &tick(result),
            "rjwalters/loom".to_string(),
            RepoVisibility::Private,
            Some(poisoned.clone()),
        );
        assert_eq!(record.result, result);
        assert!(!result.spawned(), "{result:?} launched no session");
        assert_eq!(record.tokens_by_model, None, "{result:?} consumed no tokens");
        assert_eq!(record.models_used, None, "{result:?}");
        assert_eq!(record.actions, None, "{result:?}");
    }
}

/// `LoadSkipped` is the one "skipped" variant that DID spawn — it was
/// terminated at the wall-clock ceiling — so its transcript is real and must
/// be attributed.
#[test]
fn build_record_attributes_a_load_skipped_tick_which_did_spawn() {
    assert!(RoleTickResult::SkippedLoad.spawned());
    let record = build_record(
        &tick(RoleTickResult::SkippedLoad),
        "rjwalters/loom".to_string(),
        RepoVisibility::Private,
        Some(TranscriptScan {
            tokens_by_model: vec![totals("claude-sonnet-5", 7, 8)],
            actions: RoleTickActions::default(),
        }),
    );
    assert_eq!(record.models_used, Some(vec!["claude-sonnet-5".to_string()]));
}

#[test]
fn build_record_omits_an_unresolved_model_and_effort() {
    let mut t = tick(RoleTickResult::SkippedNoTokenPool);
    t.model = None;
    t.effort = Some(String::new());
    t.detail = Some("no-token-pool".to_string());
    let record = build_record(&t, "rjwalters/loom".to_string(), RepoVisibility::Private, None);
    assert_eq!(record.model, None);
    assert_eq!(record.effort, None, "an empty effort is unset, not \"\"");
    assert_eq!(record.detail.as_deref(), Some("no-token-pool"));
    let json = serde_json::to_value(&record).unwrap();
    assert!(json.get("model").is_none());
    assert!(json.get("effort").is_none());
}

#[test]
fn build_record_clamps_a_negative_duration_to_zero() {
    let mut t = tick(RoleTickResult::Success);
    t.ended_at = at(-5);
    let record = build_record(&t, "r/r".to_string(), RepoVisibility::Private, None);
    assert_eq!(
        record.duration_sec, 0,
        "a clock step backwards must not publish a negative duration"
    );
}

// ------------------------------------------------------------------------
// Integration: one tick, end to end, answered from the journal alone
// ------------------------------------------------------------------------

/// Stage a fake `$CLAUDE_CONFIG_DIR/projects/<slug>/<uuid>.jsonl` for `root`
/// and point the role-tick journal at a temp file. Returns the journal path.
fn stage_tick_env(dir: &Path, root: &Path, role: &str, transcript: &str) -> PathBuf {
    let config = dir.join("claude-config");
    let project = config
        .join("projects")
        .join(crate::transcript_tokens::project_slug(root));
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("session-uuid.jsonl"), transcript).unwrap();
    let journal = dir.join("role-tick-telemetry.jsonl");
    std::env::set_var("CLAUDE_CONFIG_DIR", &config);
    std::env::set_var(crate::sweep_outcomes::ROLE_TICK_TELEMETRY_JOURNAL_PATH_ENV, &journal);
    let _ = role; // the transcript already names it; kept for call-site clarity
    journal
}

fn clear_tick_env() {
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var(crate::sweep_outcomes::ROLE_TICK_TELEMETRY_JOURNAL_PATH_ENV);
}

/// Issue #8056 test plan, integration case: drive a role tick to its terminal
/// outcome and assert the emitted record answers "what did this tick cost?"
/// and "what did it do?" **without touching any other file** — no
/// `sweep-outcomes.jsonl` join, no transcript re-read by the consumer.
#[test]
#[serial_test::serial]
fn a_successful_tick_writes_a_self_sufficient_journal_record() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();

    let mut transcript = role_head("judge");
    transcript.push_str(&usage_line("claude-sonnet-5", 900, 120));
    transcript.push_str(&bash_line("gh issue edit 42 --add-label loom:pr"));
    transcript.push_str(&bash_line("gh pr comment 42 --body approved"));
    let journal = stage_tick_env(dir.path(), &root, "judge", &transcript);

    emit_for_tick(
        &root,
        "judge",
        Utc::now(),
        &RoleTickOutcome::Success,
        Some(("claude-sonnet-5".to_string(), "high".to_string())),
    );

    let records = crate::sweep_outcomes::read_all_role_tick_outcomes(&journal);
    clear_tick_env();

    assert_eq!(records.len(), 1, "exactly one record per tick");
    let r = &records[0];
    assert_eq!(r.role, "judge");
    assert_eq!(r.result, RoleTickResult::Success);
    assert_eq!(r.model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(r.effort.as_deref(), Some("high"));
    assert_eq!(r.detail, None);
    // No `origin` remote in a bare temp dir: the repo falls back to the
    // workspace path and the visibility tag stays Private (fail-closed).
    assert_eq!(r.repo, root.display().to_string());
    assert_eq!(r.visibility, RepoVisibility::Private);
    // "What did it cost?" — answered from this record alone.
    let rows = r.tokens_by_model.as_ref().expect("tokens attributed");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].input, rows[0].output), (900, 120));
    assert_eq!(r.models_used, Some(vec!["claude-sonnet-5".to_string()]));
    // "What did it do?" — likewise.
    let actions = r.actions.as_ref().expect("actions observed");
    assert_eq!(actions.issues_labeled, 1);
    assert_eq!(actions.comments_posted, 1);
    assert_eq!(actions.prs_merged, 0);
}

/// The counterpart: a pre-spawn skip writes a record too — the skip class is
/// the point — but must not borrow the neighbouring transcript's numbers.
#[test]
#[serial_test::serial]
fn a_pool_exhausted_skip_writes_a_record_without_borrowing_tokens() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir_all(&root).unwrap();

    let mut transcript = role_head("champion");
    transcript.push_str(&usage_line("claude-sonnet-5", 5_000, 500));
    let journal = stage_tick_env(dir.path(), &root, "champion", &transcript);

    emit_for_tick(
        &root,
        "champion",
        Utc::now(),
        &RoleTickOutcome::PoolExhausted {
            total: 4,
            next_clear_at: at(9_000),
            pool: crate::role_runner::CredentialPool::ClaudeTokens,
            hold: crate::role_runner::PoolHold::SelfHealing,
        },
        None,
    );

    let records = crate::sweep_outcomes::read_all_role_tick_outcomes(&journal);
    clear_tick_env();

    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.result, RoleTickResult::SkippedPoolExhausted);
    assert!(r.detail.as_deref().unwrap().contains("0/4 spawnable"));
    // #8408: the Claude-pool detail keeps its pre-#8408 tag, and the record
    // now says which pool was read.
    assert!(r.detail.as_deref().unwrap().starts_with("pool-exhausted: "));
    assert_eq!(r.gated_pool.as_deref(), Some("claude_tokens"));
    assert_eq!(r.model, None, "no model was resolved before the bail-out");
    assert_eq!(
        r.tokens_by_model, None,
        "a skip that never spawned must not inherit a neighbouring session's tokens"
    );
    assert_eq!(r.models_used, None);
    assert_eq!(r.actions, None);
}

#[test]
fn classify_maps_every_outcome_variant_to_its_own_result() {
    use crate::role_runner::ModelRuntimeMismatch;
    use crate::runtime_admission::RuntimeRejection;

    let cases: Vec<(RoleTickOutcome, RoleTickResult)> = vec![
        (RoleTickOutcome::Success, RoleTickResult::Success),
        (RoleTickOutcome::Failure("boom".to_string()), RoleTickResult::Failure),
        (RoleTickOutcome::NoTokenPool, RoleTickResult::SkippedNoTokenPool),
        (
            RoleTickOutcome::PoolExhausted {
                total: 2,
                next_clear_at: at(10),
                pool: crate::role_runner::CredentialPool::ClaudeTokens,
                hold: crate::role_runner::PoolHold::SelfHealing,
            },
            RoleTickResult::SkippedPoolExhausted,
        ),
        (
            RoleTickOutcome::ModelRuntimeMismatch(ModelRuntimeMismatch {
                role: "judge".to_string(),
                runtime: "codex".to_string(),
                model: "sonnet".to_string(),
                model_source: "config".to_string(),
                reason: "claude-shaped model on codex".to_string(),
            }),
            RoleTickResult::SkippedModelRuntimeMismatch,
        ),
        (
            RoleTickOutcome::LoadSkipped {
                load_per_core: 4.25,
                detail: "tail".to_string(),
            },
            RoleTickResult::SkippedLoad,
        ),
        (
            RoleTickOutcome::RuntimeRejected(RuntimeRejection {
                role: "judge".to_string(),
                runtime: "codex".to_string(),
                source: crate::runtime_admission::RuntimeSource::RoleConfig,
                unmet_capabilities: vec!["subagents".to_string()],
                reason: "not admitted".to_string(),
            }),
            RoleTickResult::RuntimeRejected,
        ),
    ];
    for (outcome, expected) in cases {
        let (result, detail) = classify(&outcome);
        assert_eq!(result, expected, "{outcome:?}");
        assert_eq!(
            detail.is_none(),
            matches!(outcome, RoleTickOutcome::Success),
            "only a success carries no detail ({outcome:?})"
        );
    }
}

/// #8408 AC: the `role_tick.outcome` record names the pool that gated a
/// pre-spawn pool skip — and for a codex-pinned role that is the codex account
/// pool, never Claude's. Every other result leaves the key absent.
#[test]
fn a_pool_skip_record_names_the_pool_that_gated_it() {
    let codex = RoleTickOutcome::PoolExhausted {
        total: 4,
        next_clear_at: at(900),
        pool: crate::role_runner::CredentialPool::CodexAccounts,
        hold: crate::role_runner::PoolHold::SelfHealing,
    };
    let (result, detail) = classify(&codex);
    assert_eq!(result, RoleTickResult::SkippedPoolExhausted);
    assert!(detail
        .as_deref()
        .unwrap()
        .starts_with("codex-account-pool-exhausted: 0/4 spawnable"));

    let mut skipped = tick(result);
    skipped.detail = detail;
    skipped.gated_pool = codex.gated_pool().map(str::to_string);
    let record = build_record(&skipped, "o/r".to_string(), RepoVisibility::Private, None);
    assert_eq!(record.gated_pool.as_deref(), Some("codex_accounts"));
    let wire = serde_json::to_value(&record).unwrap();
    assert_eq!(wire["gated_pool"], "codex_accounts");

    let success = build_record(
        &tick(RoleTickResult::Success),
        "o/r".to_string(),
        RepoVisibility::Private,
        None,
    );
    assert_eq!(success.gated_pool, None);
    assert!(serde_json::to_value(&success)
        .unwrap()
        .get("gated_pool")
        .is_none());
}
