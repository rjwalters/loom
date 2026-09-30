//! Contract tests for the #9465 halves of `sweep.outcome` that PR #9481
//! shipped without record-level coverage: `pr_numbers`, and `model` naming the
//! model that **actually ran** while `config["arm"]` keeps the dispatch-time
//! experiment arm.
//!
//! These are deliberately end-to-end over
//! [`SweepRegistry::append_outcome_telemetry_journal`] rather than unit tests
//! of the two derivations in isolation. The failure mode that matters is an
//! **ordering** one: `config["arm"]` is inserted from the dispatched `model`
//! *before* `model` is reassigned to the dominant actually-run model, so a
//! future refactor that moves either statement past the other silently
//! re-derives the arm from the post-swap value — an opus-escalated sonnet
//! dispatch would then be re-attributed from Arm B to Arm A, corrupting every
//! model-cost comparison drawn from the journal. Only a test that exercises
//! both fields on one record can see that.
//!
//! In their own sibling module for the same file-size reason `timeline_tests`,
//! `phase_usage_tests` and `tokens_status_tests` already are.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::time::Duration;
use tempfile::tempdir;

/// One assistant usage record attributed to `model`, with a distinct
/// `message.id` so the fold counts it as its own message.
fn usage_line(id: &str, model: &str, input: i64, output: i64) -> String {
    // #9454: per-record attribution keys on each record's timestamp, so the
    // fixture stamps it now — inside the resolve window (which ends at the
    // reaper's `now`, after this seeding).
    let iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"{iso}\",\"message\":{{\"id\":\"{id}\",\"model\":\"{model}\",\
         \"usage\":{{\"input_tokens\":{input},\"output_tokens\":{output},\
         \"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n"
    )
}

/// Point `CLAUDE_CONFIG_DIR` at a fresh tree and write one `/loom:sweep <issue>`
/// session transcript holding `lines` — the same seam `phase_usage_tests` uses.
fn seed_sweep_transcript(root: &Path, registry: &SweepRegistry, issue: u32, lines: &str) {
    let config_dir = root.join("claude-config");
    std::fs::create_dir_all(config_dir.join("projects")).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);
    let project = config_dir
        .join("projects")
        .join(crate::transcript_tokens::project_slug(&registry.config.workspace_root));
    std::fs::create_dir_all(&project).unwrap();
    let head = format!(
        "{{\"type\":\"user\",\"message\":{{\"content\":\
         \"<command-name>/loom:sweep</command-name>\\n\
         <command-args>{issue} --claim-owned {issue}</command-args>\"}}}}\n"
    );
    std::fs::write(project.join("uuid-1.jsonl"), format!("{head}{lines}")).unwrap();
}

/// Overwrite the sampled transition history with `(phase, pr_number)` marks, in
/// lifecycle order. The reaper stamps its own instants on a real observation,
/// which a test cannot control — the history itself is the honest seam.
fn seed_phase_history_with_prs(
    registry: &mut SweepRegistry,
    sweep_id: &str,
    started_at: DateTime<Utc>,
    marks: &[(&str, Option<u32>)],
) {
    registry.phase_history.insert(
        sweep_id.to_string(),
        marks
            .iter()
            .enumerate()
            .map(|(i, (phase, pr_number))| PhaseObservation {
                phase: (*phase).to_string(),
                at: started_at + chrono::Duration::seconds(60 * (i as i64 + 1)),
                pr_number: *pr_number,
                jev: None,
            })
            .collect(),
    );
}

fn outcome_for(registry: &SweepRegistry, issue: u32) -> telemetry::SweepOutcomeRecord {
    let path = registry.config().resolve_outcome_telemetry_path();
    sweep_outcomes::read_all_sweep_outcomes(&path)
        .into_iter()
        .find(|r| r.issue == issue)
        .expect("terminal transition must journal a sweep.outcome record")
}

/// The raw JSON of the `sweep.outcome` line — the only way to tell an
/// **omitted** optional field from a `null` one.
fn raw_outcome(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).expect("telemetry journal must exist");
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["record"]["kind"] == "sweep.outcome" && v["record"]["issue"] == issue)
        .expect("a sweep.outcome line for this issue")["record"]
        .clone()
}

/// AC (a): `pr_numbers` lists **every** PR the sweep's lifecycle carried, in
/// first-seen order — not only the latest, which is what `pr_number` reports.
/// This is the multi-PR slice shape #9465 measured (20 issues landed through
/// more than one merged PR; one through 47), and the reason a single
/// latest-`pr_number` under-counts per-issue work.
#[test]
fn pr_numbers_lists_every_sampled_pr_in_first_seen_order() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9465;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    // A parked-then-re-dispatched sweep: PR #100 opened, superseded by #90
    // (a LOWER number — so a test that accidentally sorted would pass for the
    // wrong reason), then re-observed as #100 on a later tick.
    seed_phase_history_with_prs(
        &mut registry,
        &sweep_id,
        started_at,
        &[
            ("curator-done", None),
            ("builder-done", Some(100)),
            ("judge-rejected", Some(100)),
            ("doctor-done", Some(90)),
            ("judge-done", Some(100)),
        ],
    );

    registry.reap_once();

    let record = outcome_for(&registry, issue);
    assert_eq!(
        record.pr_numbers.as_deref(),
        Some([100, 90].as_slice()),
        "first-seen order, deduped — never sorted, never only the latest"
    );
    assert_eq!(
        record.pr_number,
        Some(100),
        "`pr_number` stays the LATEST observation, which the timeline fields key off"
    );
}

/// AC (a), absence case: a sweep that never carried a PR omits `pr_numbers`
/// entirely rather than reporting `[]` — "no PR observed" and "a PR was
/// observed and there were none" must stay distinguishable on the wire, the
/// same contract every other optional field on this record keeps.
#[test]
fn a_sweep_that_never_opened_a_pr_omits_pr_numbers() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9467;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-2", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    seed_phase_history_with_prs(
        &mut registry,
        &sweep_id,
        started_at,
        &[("curator-done", None), ("builder-done", None)],
    );

    registry.reap_once();

    assert_eq!(outcome_for(&registry, issue).pr_numbers, None);
    let raw = raw_outcome(&registry, issue);
    assert!(raw.get("pr_numbers").is_none(), "the key is omitted, not null/[]: {raw}");
}

/// AC (b) + (c), on one record: a Doctor-escalated sweep dispatched on
/// `sonnet` whose transcripts are dominated by `claude-opus-5` reports
/// `model: "claude-opus-5"` (what actually ran) while `config["arm"]` stays
/// `"B"` (what was dispatched).
///
/// `"B"` is the whole point. `infer_arm_from_model` maps the opus family to
/// `"A"`, so if the arm were derived from the reassigned `model` this assertion
/// would read `"A"` — the test fails loudly on the ordering regression instead
/// of silently agreeing with either implementation.
#[test]
#[serial]
fn model_names_the_dominant_actual_model_while_arm_keeps_the_dispatched_one() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9468;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-3", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    {
        let info = registry.entries.get_mut(&sweep_id).unwrap();
        info.model = Some("sonnet".to_string());
        info.started_at = started_at;
    }
    // Sonnet ran the early phases; the Doctor ladder escalated to Opus, which
    // spent the majority of the tokens. Neither row is the majority on BOTH
    // axes on its own — sonnet has more input-only spend — so the fixture also
    // pins that dominance is decided on input+output, not input alone.
    let lines = [
        usage_line("curator", "claude-sonnet-5", 12_000, 500),
        usage_line("builder", "claude-sonnet-5", 6_000, 400),
        usage_line("doctor", "claude-opus-5", 17_000, 9_000),
    ]
    .concat();
    seed_sweep_transcript(dir.path(), &registry, issue, &lines);

    registry.reap_once();
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let record = outcome_for(&registry, issue);
    assert_eq!(
        record.model.as_deref(),
        Some("claude-opus-5"),
        "`model` is the dominant (input+output) entry in tokens_by_model"
    );
    assert_eq!(
        record.config.get("arm").map(String::as_str),
        Some("B"),
        "#4809's arm is inferred from the DISPATCHED model (sonnet -> B), not from \
         the reassigned `model` (claude-opus-5 -> A)"
    );
    assert_eq!(
        record.models_used.as_deref(),
        Some(["claude-opus-5".to_string(), "claude-sonnet-5".to_string()].as_slice()),
        "`models_used` is what still shows the earlier phases ran something else"
    );
    let sonnet_total: i64 = record
        .tokens_by_model
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|row| row.model == "claude-sonnet-5")
        .map(|row| row.input + row.output)
        .sum();
    assert_eq!(sonnet_total, 18_900, "18,000 input + 900 output across two sonnet phases");
}

/// AC (b), fallback case: with no attributable transcript there is nothing to
/// be dominant, so `model` falls back to the **dispatched** model rather than
/// becoming absent. The arm is unchanged by the fallback — the two come from
/// the same value in this shape, which is exactly why the escalated fixture
/// above is the one that discriminates.
#[test]
#[serial]
fn model_falls_back_to_the_dispatched_model_when_nothing_was_attributed() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 9469;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-4", "log\n");
    {
        let info = registry.entries.get_mut(&sweep_id).unwrap();
        info.model = Some("sonnet".to_string());
    }
    // An empty transcript tree: a sweep that died before writing any usage.
    seed_sweep_transcript(dir.path(), &registry, issue, "");

    registry.reap_once();
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let record = outcome_for(&registry, issue);
    assert_eq!(record.model.as_deref(), Some("sonnet"));
    assert_eq!(record.config.get("arm").map(String::as_str), Some("B"));
    assert_eq!(record.models_used, None, "no attributable transcript: omitted, never []");
}
