//! Issue #9440: the live event-bus path's terminal records carry token
//! counters and a `tokens_status`.
//!
//! This path emitted the **majority** of the fleet's `sweep.outcome` records
//! and hard-coded `tokens_in: None` on every one of them, which is the larger
//! half of the 10.3% measurement rate #9440 found. The read is a post-pass
//! ([`super::attach_outcome_usage`]) rather than part of
//! [`super::map_event_to_records`], so the mapping stays pure and the disk
//! scan stays off the collector's reactor thread — both properties are
//! asserted here.
//!
//! In their own sibling module so `collector.rs`'s existing test file does not
//! grow (`scripts/check-file-size-budget.sh`).

use super::*;
use serial_test::serial;
use tempfile::TempDir;

struct ClaudeStore {
    _dir: TempDir,
    projects: PathBuf,
    previous: Option<std::ffi::OsString>,
}

impl ClaudeStore {
    fn seed() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        Self {
            _dir: dir,
            projects,
            previous,
        }
    }

    fn seed_sweep_session(&self, workspace: &Path, issue: u32, input: i64, output: i64) {
        let dir = self
            .projects
            .join(crate::transcript_tokens::project_slug(workspace));
        std::fs::create_dir_all(&dir).unwrap();
        // #9454: per-record attribution keys on each record's timestamp, so
        // the fixture stamps it now (inside every reconstructed window).
        let iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let head = format!(
            "{{\"type\":\"user\",\"timestamp\":\"{iso}\",\"message\":{{\"content\":\
             \"<command-name>/loom:sweep</command-name>\\n\
             <command-args>{issue}</command-args>\"}}}}\n"
        );
        let usage = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{iso}\",\"message\":{{\"model\":\"claude-sonnet-5\",\
             \"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\
             \"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n"
        );
        std::fs::write(dir.join("session-uuid.jsonl"), format!("{head}{usage}")).unwrap();
    }
}

impl Drop for ClaudeStore {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
    }
}

fn terminal_envelopes(issue: u32, duration_sec: i64) -> Vec<TelemetryEnvelope> {
    map_event_to_records(
        &Event::SweepExited {
            issue,
            exit_code: Some(1),
            duration_sec,
            no_progress: false,
            death_class: None,
            repo: None,
        },
        issue,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut HashMap::new(),
    )
    .into_iter()
    .map(|record| TelemetryEnvelope::new("host-test", record))
    .collect()
}

fn outcome_of(envelopes: &[TelemetryEnvelope]) -> &crate::telemetry::SweepOutcomeRecord {
    envelopes
        .iter()
        .find_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepOutcome(record) => Some(record),
            _ => None,
        })
        .expect("a sweep.outcome record")
}

#[test]
fn the_mapping_itself_stays_pure_and_leaves_the_token_fields_unset() {
    // The purity property the post-pass design exists to preserve: no
    // workspace root is even in scope here, so nothing can be read.
    let envelopes = terminal_envelopes(11, 300);
    let outcome = outcome_of(&envelopes);
    assert_eq!(outcome.tokens_in, None);
    assert_eq!(outcome.tokens_status, None);
}

#[tokio::test]
#[serial]
async fn a_failed_sweeps_live_record_now_carries_the_tokens_it_burned() {
    let store = ClaudeStore::seed();
    let workspace = tempfile::tempdir().unwrap();
    let issue = 9450;
    store.seed_sweep_session(workspace.path(), issue, 2_400, 310);

    let mut envelopes = terminal_envelopes(issue, 600);
    attach_outcome_usage(&mut envelopes, workspace.path(), issue, None).await;

    let outcome = outcome_of(&envelopes);
    assert_eq!(outcome.result, SweepResult::Failure);
    assert_eq!(outcome.tokens_status, Some(crate::telemetry::TokensStatus::Measured));
    assert_eq!(outcome.tokens_in, Some(2_400));
    assert_eq!(outcome.tokens_out, Some(310));
    assert_eq!(outcome.models_used.as_deref(), Some(["claude-sonnet-5".to_string()].as_slice()));

    // The paired `sweep.completed` record carries the SAME breakdown, from the
    // same single read — never a second scan that could disagree.
    let completed = envelopes
        .iter()
        .find_map(|envelope| match &envelope.record {
            TelemetryRecord::SweepCompleted(record) => Some(record),
            _ => None,
        })
        .expect("a sweep.completed record");
    assert_eq!(completed.tokens_by_model, outcome.tokens_by_model);
}

/// `Σ phase_durations[*] + tokens_unattributed == tokens_in/tokens_out` —
/// [`crate::telemetry::SweepOutcomeRecord::tokens_unattributed`]'s own
/// documented invariant, asserted rather than eyeballed (Issue #9486).
/// Panics with the offending record on either axis.
fn assert_tokens_reconcile(outcome: &crate::telemetry::SweepOutcomeRecord) {
    let (Some(total_in), Some(total_out)) = (outcome.tokens_in, outcome.tokens_out) else {
        assert_eq!(
            outcome.tokens_unattributed, None,
            "with no totals there is nothing to take a remainder of: {outcome:?}"
        );
        return;
    };
    let (attributed_in, attributed_out) = outcome
        .phase_durations
        .iter()
        .filter_map(crate::telemetry::PhaseDuration::token_split)
        .fold((0u64, 0u64), |(sum_in, sum_out), (tokens_in, tokens_out)| {
            (sum_in + tokens_in, sum_out + tokens_out)
        });
    let remainder = outcome.tokens_unattributed.as_ref().unwrap_or_else(|| {
        panic!("a record with known totals must carry a remainder: {outcome:?}")
    });
    assert_eq!(attributed_in + remainder.tokens_in, total_in, "tokens_in");
    assert_eq!(attributed_out + remainder.tokens_out, total_out, "tokens_out");
}

#[tokio::test]
#[serial]
async fn a_measured_record_with_no_phase_attribution_carries_the_whole_total_as_unattributed() {
    // Issue #9486 (absorbed into #9477): #9440 filled the totals on this path
    // but left `tokens_unattributed` at `None`, while `terminal_records`
    // builds an empty `phase_durations` and nothing else fills it — so every
    // record this path emits violated the schema's documented invariant. An
    // empty Σ means the WHOLE measured total is the remainder.
    let store = ClaudeStore::seed();
    let workspace = tempfile::tempdir().unwrap();
    let issue = 9477;
    store.seed_sweep_session(workspace.path(), issue, 4_200, 730);

    let mut envelopes = terminal_envelopes(issue, 600);
    attach_outcome_usage(&mut envelopes, workspace.path(), issue, None).await;

    let outcome = outcome_of(&envelopes);
    assert_eq!(outcome.tokens_status, Some(crate::telemetry::TokensStatus::Measured));
    assert!(outcome.phase_durations.is_empty(), "this path samples no per-phase windows");
    assert_eq!(outcome.tokens_in, Some(4_200));
    assert_eq!(outcome.tokens_out, Some(730));
    assert_eq!(
        outcome.tokens_unattributed,
        Some(crate::telemetry::TokenTotals {
            tokens_in: 4_200,
            tokens_out: 730,
        }),
    );
    assert_tokens_reconcile(outcome);
}

#[tokio::test]
#[serial]
async fn a_spawn_death_on_the_live_path_is_a_measured_zero() {
    let store = ClaudeStore::seed();
    let workspace = tempfile::tempdir().unwrap();
    let issue = 9451;
    // An earlier dispatch's tokens are on disk; this attempt never ran.
    store.seed_sweep_session(workspace.path(), issue, 5_000, 600);

    let mut envelopes = terminal_envelopes(issue, 4);
    attach_outcome_usage(
        &mut envelopes,
        workspace.path(),
        issue,
        Some("preflight-no-cli-start".to_string()),
    )
    .await;

    let outcome = outcome_of(&envelopes);
    assert_eq!(outcome.tokens_status, Some(crate::telemetry::TokensStatus::NotSpawned));
    assert_eq!(outcome.tokens_in, Some(0));
    assert_eq!(outcome.tokens_out, Some(0));
    assert_eq!(outcome.tokens_status_reason.as_deref(), Some("preflight-no-cli-start"));
    // #9486: a *measured* zero is a known total, so it still reconciles —
    // `Some(0, 0)`, since zero of zero really is accounted for. Only an
    // *unknown* total omits the remainder.
    assert_eq!(
        outcome.tokens_unattributed,
        Some(crate::telemetry::TokenTotals {
            tokens_in: 0,
            tokens_out: 0,
        }),
    );
    assert_tokens_reconcile(outcome);
}

#[tokio::test]
#[serial]
async fn a_sweep_with_no_transcript_is_unattributable_with_a_reason() {
    let _store = ClaudeStore::seed();
    let workspace = tempfile::tempdir().unwrap();

    let mut envelopes = terminal_envelopes(9452, 600);
    attach_outcome_usage(&mut envelopes, workspace.path(), 9452, None).await;

    let outcome = outcome_of(&envelopes);
    assert_eq!(outcome.tokens_status, Some(crate::telemetry::TokensStatus::Unattributable));
    assert_eq!(outcome.tokens_in, None, "never a fabricated zero");
    assert_eq!(
        outcome.tokens_status_reason.as_deref(),
        Some(crate::sweep_usage::REASON_NO_TRANSCRIPT)
    );
    // #9486: unknown totals leave the remainder absent — `Some(0, 0)` would
    // claim the (empty) per-phase breakdown accounts for everything.
    assert_eq!(outcome.tokens_unattributed, None);
    assert_tokens_reconcile(outcome);
}

#[tokio::test]
#[serial]
async fn a_terminal_event_with_no_dispatch_state_declines_rather_than_guessing() {
    let store = ClaudeStore::seed();
    let workspace = tempfile::tempdir().unwrap();
    let issue = 9453;
    store.seed_sweep_session(workspace.path(), issue, 800, 90);

    // `duration_sec: 0` is what a `SweepCrashed` with no tracked dispatch
    // yields. Reading unbounded would fold every earlier dispatch of this
    // issue into one record.
    let mut envelopes = terminal_envelopes(issue, 0);
    attach_outcome_usage(&mut envelopes, workspace.path(), issue, None).await;

    let outcome = outcome_of(&envelopes);
    assert_eq!(outcome.tokens_status, Some(crate::telemetry::TokensStatus::Unattributable));
    assert_eq!(
        outcome.tokens_status_reason.as_deref(),
        Some(crate::sweep_usage::REASON_NO_WINDOW)
    );
}

#[tokio::test]
async fn a_non_terminal_event_reads_nothing_at_all() {
    // A phase event produces no `sweep.outcome`, so the post-pass must not
    // even reach the blocking hop — the per-phase cost stays exactly zero.
    let mut envelopes: Vec<TelemetryEnvelope> = map_event_to_records(
        &Event::SweepPhase {
            issue: 3,
            phase: "builder".to_string(),
            pr_number: None,
            repo: None,
        },
        3,
        "rjwalters/loom",
        RepoVisibility::Private,
        &mut HashMap::new(),
    )
    .into_iter()
    .map(|record| TelemetryEnvelope::new("host-test", record))
    .collect();
    let before = envelopes.clone();
    attach_outcome_usage(&mut envelopes, Path::new("/nonexistent"), 3, None).await;
    assert_eq!(envelopes, before);
}

#[test]
fn death_class_precedence_matches_the_durable_journals() {
    // `death_class` (the pre-flight classifier) outranks the crash
    // classification, exactly as `append_outcome_journal` orders them.
    let both = Event::SweepCrashed {
        issue: 1,
        checkpoint_phase: None,
        classification: Some("account-exhausted:rate-limited".to_string()),
        death_class: Some("preflight-mcp-failed".to_string()),
        repo: None,
    };
    assert_eq!(event_death_class(&both).as_deref(), Some("preflight-mcp-failed"));

    let crash_only = Event::SweepCrashed {
        issue: 1,
        checkpoint_phase: None,
        classification: Some("execution-error".to_string()),
        death_class: None,
        repo: None,
    };
    assert_eq!(event_death_class(&crash_only).as_deref(), Some("execution-error"));

    assert_eq!(
        event_death_class(&Event::SweepPhase {
            issue: 1,
            phase: "builder".to_string(),
            pr_number: None,
            repo: None,
        }),
        None
    );
}
