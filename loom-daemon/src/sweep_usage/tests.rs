//! Issue #9440: the three `tokens_status` cases, driven through the real
//! readers rather than a stub — the acceptance criterion names spawn death,
//! cancellation mid-Builder, and a missing transcript by name.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serial_test::serial;
use tempfile::TempDir;

use super::*;

/// `CLAUDE_CONFIG_DIR` is process-global, so every test that seeds a fake
/// Claude store runs `#[serial]` and restores the previous value.
struct ClaudeStore {
    _dir: TempDir,
    projects: PathBuf,
    previous: Option<std::ffi::OsString>,
}

impl ClaudeStore {
    fn seed() -> Self {
        Self::with_projects(true)
    }

    /// A `CLAUDE_CONFIG_DIR` with no `projects/` directory under it at all —
    /// the "this host has no usage store" shape, which must stay
    /// distinguishable from "the store exists and held nothing for this sweep".
    fn without_store() -> Self {
        Self::with_projects(false)
    }

    fn with_projects(create: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        if create {
            fs::create_dir_all(&projects).unwrap();
        }
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
        Self {
            _dir: dir,
            projects,
            previous,
        }
    }

    /// Write `<projects>/<slug>/<uuid>.jsonl` for a `/loom:sweep <issue>`
    /// session carrying one assistant usage record, timestamped inside the
    /// default test window (per-record attribution keys on each record's own
    /// `timestamp` — Issue #9454).
    fn seed_sweep_session(&self, workspace: &Path, issue: u32, input: i64, output: i64) {
        self.seed_session_record(
            workspace,
            issue,
            input,
            output,
            Utc::now() - chrono::Duration::minutes(1),
        );
    }

    /// Same, with an explicit record timestamp — what lets a test place one
    /// session's records inside a sweep's window and another's outside it.
    fn seed_session_record(
        &self,
        workspace: &Path,
        issue: u32,
        input: i64,
        output: i64,
        at: chrono::DateTime<Utc>,
    ) {
        let dir = self
            .projects
            .join(crate::transcript_tokens::project_slug(workspace));
        fs::create_dir_all(&dir).unwrap();
        let iso = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
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
        let name = format!("session-{}.jsonl", at.timestamp_nanos_opt().unwrap_or_default());
        fs::write(dir.join(name), format!("{head}{usage}")).unwrap();
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

fn recent_window() -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let now = Utc::now();
    Some((now - chrono::Duration::hours(1), now))
}

// ---------------------------------------------------------------------------
// `not_spawned`: a true, measured zero.
// ---------------------------------------------------------------------------

#[test]
fn every_preflight_class_and_the_pool_fault_are_never_spawned() {
    for class in [
        "preflight-no-cli-start",
        "preflight-mcp-failed",
        "preflight-token-selection-failed",
        "no-usable-account",
    ] {
        assert_eq!(
            never_spawned_reason(Some(class)),
            Some(class),
            "{class} must count as a spawn death"
        );
    }
}

#[test]
fn an_exhausted_account_is_not_a_spawn_death() {
    // The CLI printed the exhaustion signature, so it ran — and very often
    // burned tokens first. Calling this a zero re-creates the undercount.
    for class in [
        "account-exhausted:rate-limited",
        "account-exhausted:model-credits-exhausted",
        "execution-error",
        "exit-1",
    ] {
        assert_eq!(never_spawned_reason(Some(class)), None, "{class} must not read as a zero");
    }
    assert_eq!(never_spawned_reason(None), None);
}

#[test]
#[serial]
fn spawn_death_reports_a_measured_zero_and_never_reads_the_store() {
    let store = ClaudeStore::seed();
    let workspace = Path::new("/workspace/spawn-death");
    // Tokens on disk from an EARLIER dispatch of the same issue must not be
    // folded in: this attempt never ran.
    store.seed_sweep_session(workspace, 77, 5_000, 400);

    let usage =
        resolve(None, workspace, 77, recent_window(), Some("preflight-token-selection-failed"));

    assert_eq!(usage.status, crate::telemetry::TokensStatus::NotSpawned);
    assert_eq!(usage.tokens_in, Some(0));
    assert_eq!(usage.tokens_out, Some(0));
    assert_eq!(usage.tokens_by_model, None, "a sweep that never ran has no model to attribute");
    assert_eq!(usage.reason.as_deref(), Some("preflight-token-selection-failed"));
}

// ---------------------------------------------------------------------------
// `measured`: including the partial read a cancelled sweep leaves behind.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn cancellation_mid_builder_reports_the_partial_usage_it_really_burned() {
    let store = ClaudeStore::seed();
    let workspace = Path::new("/workspace/cancelled");
    store.seed_sweep_session(workspace, 88, 1_200, 340);

    // A cancellation carries no failure class at all (`finish_cancel` passes
    // `death_class: None` — a manual cancel is never a pre-flight death).
    let usage = resolve(None, workspace, 88, recent_window(), None);

    assert_eq!(usage.status, crate::telemetry::TokensStatus::Measured);
    assert_eq!(usage.tokens_in, Some(1_200));
    assert_eq!(usage.tokens_out, Some(340));
    assert_eq!(usage.reason, None, "a measurement needs no excuse");
    let rows = usage.tokens_by_model.expect("per-model breakdown");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "claude-sonnet-5");
}

/// Issue #9454's acceptance: two sweeps of the same issue run back-to-back
/// on one host, and each `resolve` reports only its own window's usage —
/// an older session's history is never folded into a later sweep's
/// window, however recently the older file was last written.
#[test]
#[serial]
fn two_back_to_back_sweeps_never_fold_each_others_sessions() {
    let store = ClaudeStore::seed();
    let workspace = Path::new("/workspace/back-to-back");

    // Sweep 1's session wrote a big record two hours ago; sweep 2's wrote
    // a small record one minute ago. Both FILES now have fresh mtimes
    // (the second write refreshed the first file's), which is exactly the
    // shape that made the old whole-file attribution fold sweep 1's
    // history into sweep 2.
    store.seed_session_record(
        workspace,
        9454,
        5_000_000,
        40_000,
        Utc::now() - chrono::Duration::hours(2),
    );
    store.seed_session_record(
        workspace,
        9454,
        80_000,
        2_000,
        Utc::now() - chrono::Duration::minutes(1),
    );

    let now = Utc::now();
    let sweep1 = resolve(
        None,
        workspace,
        9454,
        Some((now - chrono::Duration::hours(3), now - chrono::Duration::hours(1))),
        None,
    );
    let sweep2 =
        resolve(None, workspace, 9454, Some((now - chrono::Duration::minutes(30), now)), None);

    assert_eq!(sweep1.status, crate::telemetry::TokensStatus::Measured);
    assert_eq!(sweep1.tokens_in, Some(5_000_000), "sweep 1 sees only its own window's record");
    assert_eq!(sweep2.status, crate::telemetry::TokensStatus::Measured);
    assert_eq!(
        sweep2.tokens_in,
        Some(80_000),
        "sweep 2 must not inherit sweep 1's history (#9454)"
    );
    assert_eq!(sweep2.tokens_out, Some(2_000));
}

#[test]
fn flatten_folds_every_billing_input_counter_onto_tokens_in() {
    let rows = vec![
        ModelUsageTotals {
            model: "a".into(),
            speed: "standard".into(),
            service_tier: "standard".into(),
            input: 10,
            cache_read: 100,
            cache_write_5m: 5,
            cache_write_1h: 7,
            output: 3,
        },
        ModelUsageTotals {
            model: "b".into(),
            speed: "standard".into(),
            service_tier: "standard".into(),
            input: 1,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 2,
        },
    ];
    // 10+100+5+7 + 1 = 123 in; 3 + 2 = 5 out.
    assert_eq!(flatten(&rows), (123, 5));
}

// ---------------------------------------------------------------------------
// `unattributable`: spawned, but the usage could not be read.
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn a_missing_transcript_is_unattributable_not_zero() {
    let _store = ClaudeStore::seed(); // store exists, this sweep has nothing in it
    let workspace = Path::new("/workspace/pruned");

    let usage = resolve(None, workspace, 99, recent_window(), Some("execution-error"));

    assert_eq!(usage.status, crate::telemetry::TokensStatus::Unattributable);
    assert_eq!(usage.tokens_in, None, "absent, never a fabricated zero");
    assert_eq!(usage.tokens_out, None);
    assert_eq!(usage.tokens_by_model, None);
    assert_eq!(usage.reason.as_deref(), Some(REASON_NO_TRANSCRIPT));
}

#[test]
#[serial]
fn a_host_with_no_claude_store_is_distinguishable_from_an_empty_one() {
    let _store = ClaudeStore::without_store();

    let usage = resolve(None, Path::new("/workspace/none"), 1, recent_window(), None);

    assert_eq!(usage.status, crate::telemetry::TokensStatus::Unattributable);
    assert_eq!(
        usage.reason.as_deref(),
        Some(REASON_NO_STORE),
        "an operator chasing a low measurement rate needs 'no store here' apart from 'nothing for \
         this sweep'"
    );
}

#[test]
#[serial]
fn an_explicit_claude_runtime_classifies_a_missing_store_the_same_way_an_absent_one_does() {
    // `sweep_usage_runtime` returns `Some("claude")` whenever the sweep's log
    // carries a `# LOOM_RUNTIME_RESOLVED` marker, and `None` for a spawn that
    // wrote none. Both select the SAME transcripts, so both must report the
    // same absence reason — keying the decision on `usage_runtime.is_none()`
    // instead of the selected source would silently mislabel every marked
    // Claude sweep as a pruned transcript.
    let _store = ClaudeStore::without_store();

    for runtime in [None, Some("claude")] {
        let usage = resolve(runtime, Path::new("/workspace/none"), 1, recent_window(), None);
        assert_eq!(
            usage.reason.as_deref(),
            Some(REASON_NO_STORE),
            "runtime {runtime:?} must classify a missing Claude store as such"
        );
    }
}

#[test]
#[serial]
fn a_non_claude_runtime_with_no_store_reports_the_per_sweep_absence() {
    // A Codex sweep's store is directory-scoped: the reader cannot distinguish
    // "no store on this host" from "nothing for this sweep", so it must not
    // claim the Claude-specific `no-usage-store` just because `~/.claude` is
    // missing — that would attribute a Claude fact to a Codex sweep.
    let _store = ClaudeStore::without_store();
    // Never scan the host's real Codex profile root (#9964).
    let profiles = tempfile::tempdir().unwrap();
    let _profile_root = crate::tokens_pool::profile_root_env::ProfileRootEnv::set(profiles.path());

    let usage = resolve(Some("codex"), Path::new("/workspace/none"), 1, recent_window(), None);

    assert_eq!(usage.status, crate::telemetry::TokensStatus::Unattributable);
    assert_eq!(usage.reason.as_deref(), Some(REASON_NO_TRANSCRIPT));
}

#[test]
#[serial]
fn an_unknown_start_instant_is_unattributable_rather_than_an_unbounded_read() {
    let store = ClaudeStore::seed();
    let workspace = Path::new("/workspace/windowless");
    // Tokens exist on disk — but with no window they could belong to any
    // dispatch of this issue, so reading them would double-count.
    store.seed_sweep_session(workspace, 5, 900, 100);

    let usage = resolve(None, workspace, 5, None, None);

    assert_eq!(usage.status, crate::telemetry::TokensStatus::Unattributable);
    assert_eq!(usage.tokens_in, None);
    assert_eq!(usage.reason.as_deref(), Some(REASON_NO_WINDOW));
}

// ---------------------------------------------------------------------------
// Window reconstruction.
// ---------------------------------------------------------------------------

#[test]
fn window_prefers_the_authoritative_start_instant() {
    let started_at = Utc::now() - chrono::Duration::seconds(600);
    let (start, end) = window(Some(started_at), 42).expect("window");
    assert_eq!(start, started_at, "a known start instant is never second-guessed");
    assert!(end >= started_at);
}

#[test]
fn window_reconstructs_from_a_measured_duration_when_the_entry_is_gone() {
    let (start, end) = window(None, 300).expect("window");
    let span = (end - start).num_seconds();
    assert!((299..=301).contains(&span), "reconstructed span was {span}s");
}

#[test]
fn window_declines_when_neither_end_is_known() {
    // `duration_sec == 0` is the collector's "no dispatch state" sentinel, not
    // a measurement — reconstructing a zero-width window from it would report
    // every such sweep as a fabricated zero.
    assert_eq!(window(None, 0), None);
    assert_eq!(window(None, -5), None);
}

// ── The #9454 plausibility guard ──

fn measured_usage(tokens_in: u64) -> SweepUsage {
    SweepUsage {
        tokens_in: Some(tokens_in),
        tokens_out: Some(1_000),
        tokens_by_model: None,
        status: TokensStatus::Measured,
        reason: None,
    }
}

#[test]
fn a_measured_result_over_the_ceiling_is_reclassified_suspect() {
    // 136.8M input tokens in 292s ≈ 468k/s — the #9454 evidence row
    // (rjwalters/loom#8450's judge-only landing sweep).
    let usage = apply_plausibility_guard(measured_usage(136_800_000), 292);
    assert_eq!(usage.status, TokensStatus::Suspect);
    assert_eq!(usage.reason.as_deref(), Some("implausible_input_rate"));
    // The counters are still published — flagged, not dropped.
    assert_eq!(usage.tokens_in, Some(136_800_000));
}

#[test]
fn a_plausible_measured_result_passes_through_untouched() {
    // ~32k/s — the #9440-study fleet median shape.
    let usage = apply_plausibility_guard(measured_usage(3_200_000), 100);
    assert_eq!(usage.status, TokensStatus::Measured);
    assert_eq!(usage.reason, None);
}

#[test]
fn non_measured_statuses_are_never_suspected() {
    // A preflight death's measured zero cannot become suspect, whatever
    // the duration; neither can an unattributable absence.
    let zero = SweepUsage {
        tokens_in: Some(0),
        tokens_out: Some(0),
        tokens_by_model: None,
        status: TokensStatus::NotSpawned,
        reason: Some("preflight-no-cli-start".to_string()),
    };
    assert_eq!(apply_plausibility_guard(zero, 0).status, TokensStatus::NotSpawned);

    let absent = SweepUsage::unattributable(REASON_NO_TRANSCRIPT);
    assert_eq!(apply_plausibility_guard(absent, 0).status, TokensStatus::Unattributable);
}

#[test]
fn the_ceiling_is_the_documented_100k_per_second() {
    // Exactly at the ceiling is measured; one token past it is suspect.
    assert_eq!(
        apply_plausibility_guard(measured_usage(100_000), 1).status,
        TokensStatus::Measured
    );
    assert_eq!(
        apply_plausibility_guard(measured_usage(100_001), 1).status,
        TokensStatus::Suspect
    );
    // A zero/negative duration divides by one second, not by zero.
    assert_eq!(
        apply_plausibility_guard(measured_usage(50_000), 0).status,
        TokensStatus::Measured
    );
    assert_eq!(
        apply_plausibility_guard(measured_usage(200_000), 0).status,
        TokensStatus::Suspect
    );
}
