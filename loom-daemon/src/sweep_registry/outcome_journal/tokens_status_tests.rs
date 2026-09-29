//! End-to-end tests for `sweep.outcome`'s `tokens_status` (Issue #9440) on the
//! durable journal path.
//!
//! The three-way decision itself is unit-tested in [`crate::sweep_usage`];
//! what is tested HERE is that
//! [`SweepRegistry::append_outcome_telemetry_journal`] actually *publishes* it
//! — for a `result != success` transition, which is the whole point. Before
//! #9440 a failed sweep's record simply had no token keys, so "never spawned"
//! and "spawned, transcript gone" were byte-identical on the wire.
//!
//! In their own sibling module for the same file-size reason `timeline_tests`
//! and `runtime_tests` already are.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use tempfile::{tempdir, TempDir};

/// A fake `${CLAUDE_CONFIG_DIR}` for the duration of one `#[serial]` test.
/// `claude_projects_dir()` reads that env var, so this is the only seam a
/// transcript-backed test needs — no reader is stubbed.
struct ClaudeStore {
    _dir: TempDir,
    projects: std::path::PathBuf,
    previous: Option<std::ffi::OsString>,
}

impl ClaudeStore {
    fn seed() -> Self {
        let dir = tempdir().unwrap();
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

    /// Seed a `/loom:sweep <issue>` session transcript for `workspace`,
    /// carrying one assistant usage record — the shape a sweep that got some
    /// way into the Builder and was then cancelled leaves behind.
    fn seed_partial_builder_usage(&self, workspace: &Path, issue: u32, input: i64, output: i64) {
        let dir = self
            .projects
            .join(crate::transcript_tokens::project_slug(workspace));
        std::fs::create_dir_all(&dir).unwrap();
        let head = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\
             \"<command-name>/loom:sweep</command-name>\\n\
             <command-args>{issue}</command-args>\"}}}}\n"
        );
        let usage = format!(
            "{{\"type\":\"assistant\",\"message\":{{\"model\":\"claude-sonnet-5\",\
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

/// The record as it was actually serialized, so absent-vs-zero is asserted on
/// the wire rather than on a deserialized `Option` that cannot tell an omitted
/// key from a `null`.
fn raw_record(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let text = std::fs::read_to_string(&path).unwrap();
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["record"]["issue"] == issue)
        .map(|v| v["record"].clone())
        .expect("a journaled sweep.outcome record for this issue")
}

/// A pre-flight death never exec'd the CLI, so its zero is a **measurement**.
/// This is the one status that publishes `tokens_in: 0`.
#[test]
#[serial]
fn a_spawn_death_publishes_a_measured_zero_with_the_class_as_its_reason() {
    let store = ClaudeStore::seed();
    let dir = tempdir().unwrap();
    let (registry, _rec) = fixture_registry(dir.path());
    let issue = 9440;
    // Tokens on disk from an EARLIER dispatch of the same issue: a spawn death
    // must not inherit them.
    store.seed_partial_builder_usage(dir.path(), issue, 9_000, 800);

    registry.append_outcome_telemetry_journal(
        issue,
        &format!("sweep-issue-{issue}-0"),
        7,
        telemetry::SweepResult::Failure,
        Some("preflight-token-selection-failed".to_string()),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["tokens_status"], "not_spawned");
    assert_eq!(raw["tokens_status_reason"], "preflight-token-selection-failed");
    assert_eq!(raw["tokens_in"], 0, "a spawn death's zero is measured, not absent");
    assert_eq!(raw["tokens_out"], 0);
    assert!(
        raw.get("tokens_by_model").is_none(),
        "a sweep that never ran has no model to attribute a row to: {raw}"
    );
    assert!(
        raw.get("models_used").is_none(),
        "models_used inherits tokens_by_model's contract: {raw}"
    );
}

/// A cancellation mid-Builder carries no failure class at all
/// (`finish_cancel` passes `death_class: None` — a manual cancel is never a
/// pre-flight death), and the tokens it really burned before being killed are
/// a **measurement**, not something to drop.
#[test]
#[serial]
fn a_cancelled_sweep_publishes_the_partial_usage_it_burned() {
    let store = ClaudeStore::seed();
    let dir = tempdir().unwrap();
    let (registry, _rec) = fixture_registry(dir.path());
    let issue = 9441;
    store.seed_partial_builder_usage(dir.path(), issue, 1_500, 220);

    registry.append_outcome_telemetry_journal(
        issue,
        &format!("sweep-issue-{issue}-0"),
        480,
        telemetry::SweepResult::Cancelled,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["result"], "cancelled");
    assert_eq!(raw["tokens_status"], "measured");
    assert!(
        raw.get("tokens_status_reason").is_none(),
        "a measurement needs no excuse: {raw}"
    );
    assert_eq!(raw["tokens_in"], 1_500);
    assert_eq!(raw["tokens_out"], 220);
    assert_eq!(raw["models_used"][0], "claude-sonnet-5");
}

/// A sweep whose transcript was pruned/rotated is **unattributable**: counters
/// stay absent (never coerced to zero) and the reason says which absence it
/// was. This is the case that used to be indistinguishable from a spawn death.
#[test]
#[serial]
fn a_missing_transcript_publishes_unattributable_with_a_reason_and_no_counters() {
    let _store = ClaudeStore::seed(); // store exists; nothing in it for this sweep
    let dir = tempdir().unwrap();
    let (registry, _rec) = fixture_registry(dir.path());
    let issue = 9442;

    registry.append_outcome_telemetry_journal(
        issue,
        &format!("sweep-issue-{issue}-0"),
        900,
        telemetry::SweepResult::Failure,
        Some("execution-error".to_string()),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["tokens_status"], "unattributable");
    assert_eq!(
        raw["tokens_status_reason"], "no-attributable-transcript",
        "the reason must name the absence, not restate the failure"
    );
    assert!(raw.get("tokens_in").is_none(), "never a fabricated zero: {raw}");
    assert!(raw.get("tokens_out").is_none(), "never a fabricated zero: {raw}");
    // The failure classification itself is untouched — `tokens_status` is a
    // second, independent axis, not a replacement for `failure_class`.
    assert_eq!(raw["failure_class"], "execution-error");
}

/// An account-exhaustion death DID run the CLI (that is where the signature
/// came from) and very often burned tokens first, so it must never be
/// classified as a zero — the regression that would silently re-create the
/// undercount #9440 removed.
#[test]
#[serial]
fn an_exhausted_account_reports_what_it_burned_rather_than_a_zero() {
    let store = ClaudeStore::seed();
    let dir = tempdir().unwrap();
    let (registry, _rec) = fixture_registry(dir.path());
    let issue = 9443;
    store.seed_partial_builder_usage(dir.path(), issue, 40_000, 3_100);

    registry.append_outcome_telemetry_journal(
        issue,
        &format!("sweep-issue-{issue}-0"),
        1_200,
        telemetry::SweepResult::Failure,
        Some("account-exhausted:model-credits-exhausted".to_string()),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["tokens_status"], "measured");
    assert_eq!(raw["tokens_in"], 40_000);
    assert_eq!(raw["tokens_out"], 3_100);
}

/// The window is reconstructed from the measured `duration_sec` when the
/// registry entry is already gone (a sweep reaped after a daemon restart) —
/// without it, every such record was silently token-less.
#[test]
#[serial]
fn a_sweep_with_no_registry_entry_still_measures_from_its_duration() {
    let store = ClaudeStore::seed();
    let dir = tempdir().unwrap();
    let (registry, _rec) = fixture_registry(dir.path());
    let issue = 9444;
    store.seed_partial_builder_usage(dir.path(), issue, 700, 90);

    // No `insert_*` call: the registry holds nothing for this sweep id, so
    // `started_at` is `None` and only `duration_sec` bounds the read.
    registry.append_outcome_telemetry_journal(
        issue,
        "sweep-issue-9444-0",
        360,
        telemetry::SweepResult::Failure,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["tokens_status"], "measured");
    assert_eq!(raw["tokens_in"], 700);
}

/// A `duration_sec` of `0` is a sentinel, not a measurement: with neither end
/// of the window known the record must say `unattributable`, never invent a
/// zero-width window that would report a fabricated zero.
#[test]
#[serial]
fn no_window_at_all_is_unattributable_rather_than_a_fabricated_zero() {
    let store = ClaudeStore::seed();
    let dir = tempdir().unwrap();
    let (registry, _rec) = fixture_registry(dir.path());
    let issue = 9445;
    store.seed_partial_builder_usage(dir.path(), issue, 700, 90);

    registry.append_outcome_telemetry_journal(
        issue,
        "sweep-issue-9445-0",
        0,
        telemetry::SweepResult::Failure,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["tokens_status"], "unattributable");
    assert_eq!(raw["tokens_status_reason"], "no-sweep-window");
    assert!(raw.get("tokens_in").is_none(), "never a fabricated zero: {raw}");
}
