//! Tests for [`super`] — tap-attributed usage accounting (Issue #8556).
//!
//! The stream shapes below follow `runtime-model-trials.md`'s recorded
//! observations (Pi reports usage on assistant `message_end`, OpenCode on
//! `step_finish`) rather than a vendor schema this tree can pin down; the
//! module is lenient by design and these tests fix that leniency's boundaries.

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use super::*;
use crate::launch_record::CredentialAttribution;

/// A [`TapAttribution`] built directly, for the fold tests that do not need a
/// log to parse.
fn tap(
    runtime: &str,
    profile: Option<&str>,
    source: &str,
    provider: Option<&str>,
) -> TapAttribution {
    TapAttribution {
        runtime: runtime.to_string(),
        model_profile: profile.map(str::to_string),
        credential: CredentialAttribution {
            source: source.to_string(),
            provider: provider.map(str::to_string),
            account: Some("alpha".to_string()),
        },
    }
}

/// A launch record shaped as `worker_spawn::run` writes it.
fn launch_line(tap: &str, source: &str, provider: Option<&str>) -> String {
    let mut record = serde_json::json!({
        "schema": 1,
        "tap": tap,
        "runtime": tap.split(':').next().unwrap(),
        "model": "glm-5.3",
        "credentialSource": source,
        "credentialAccount": "alpha",
    });
    if let Some(provider) = provider {
        record
            .as_object_mut()
            .unwrap()
            .insert("credentialProvider".to_string(), provider.into());
    }
    format!("# LOOM_LAUNCH {record}")
}

fn log_with(anchor: &str, body: &str) -> String {
    format!("==== loom-daemon dispatch: {anchor} issue=42 ====\nspawn-worker: go\n{body}\n")
}

// ============================================================================
// accumulate_usage
// ============================================================================

/// OpenCode's shape: usage on `step_finish`, counters under `tokens`, cache
/// counters nested one level deeper, cost on the event itself.
#[test]
fn opencode_step_finish_counters_are_summed_across_steps() {
    let stream = "\
        {\"type\":\"step_start\"}\n\
        {\"type\":\"step_finish\",\"tokens\":{\"input\":100,\"output\":20,\"reasoning\":5,\"cache\":{\"read\":900,\"write\":10}},\"cost\":0.01}\n\
        {\"type\":\"step_finish\",\"tokens\":{\"input\":50,\"output\":7,\"reasoning\":1,\"cache\":{\"read\":100,\"write\":2}},\"cost\":0.02}\n\
    ";
    let usage = accumulate_usage(stream);
    assert_eq!(usage.usage_events, 2);
    assert_eq!(usage.input, Some(150));
    assert_eq!(usage.output, Some(27));
    assert_eq!(usage.reasoning, Some(6));
    assert_eq!(usage.cache_read, Some(1000));
    assert_eq!(usage.cache_write, Some(12));
    assert!((usage.cost_estimate.unwrap() - 0.03).abs() < 1e-9);
    assert_eq!(usage.total_tokens(), Some(1195));
    assert!(usage.is_measured());
}

/// `runtime-model-trials.md`: "Pi usage appears on assistant `message_end`
/// events; count each once, not again inside `agent_end`." Counting the
/// `agent_end` repeat would double every Pi run.
#[test]
fn pi_message_end_usage_is_counted_once_and_agent_end_is_ignored() {
    let stream = "\
        {\"type\":\"message_end\",\"usage\":{\"input_tokens\":50,\"output_tokens\":7}}\n\
        {\"type\":\"agent_end\",\"usage\":{\"input_tokens\":50,\"output_tokens\":7}}\n\
    ";
    let usage = accumulate_usage(stream);
    assert_eq!(usage.usage_events, 1, "agent_end must not be folded in");
    assert_eq!(usage.input, Some(50));
    assert_eq!(usage.output, Some(7));
}

/// The rule `runtime-model-trials.md` states literally: a missing counter is
/// unmeasured, never zero. A harness reporting only input must not make this
/// module claim the launch produced no output.
#[test]
fn a_missing_counter_stays_unmeasured_rather_than_becoming_zero() {
    let usage = accumulate_usage("{\"type\":\"step_finish\",\"tokens\":{\"input\":10}}\n");
    assert_eq!(usage.input, Some(10));
    assert_eq!(usage.output, None);
    assert_eq!(usage.reasoning, None);
    assert_eq!(usage.cache_read, None);
    assert_eq!(usage.cache_write, None);
    assert_eq!(usage.cost_estimate, None);
    // A floor, not a total — and explicitly documented as such.
    assert_eq!(usage.total_tokens(), Some(10));
}

#[test]
fn a_stream_with_no_usage_bearing_events_is_unmeasured_not_zero() {
    for stream in [
        "",
        "plain prose only\n",
        "{not json\n{\"type\":123}\n{}\n",
        // A step_finish with no counters at all is not a reading of zero.
        "{\"type\":\"step_finish\"}\n{\"type\":\"tool_use\",\"tool\":\"loom_read\"}\n",
    ] {
        let usage = accumulate_usage(stream);
        assert_eq!(usage, TapUsage::default(), "{stream:?}");
        assert!(!usage.is_measured(), "{stream:?}");
        assert_eq!(usage.total_tokens(), None, "{stream:?}");
    }
}

#[test]
fn alternate_field_and_event_spellings_are_recognized() {
    let stream = "\
        {\"type\":\"step-finish\",\"usage\":{\"promptTokens\":3,\"completionTokens\":4,\"cacheReadInputTokens\":5,\"cacheCreationInputTokens\":6,\"reasoningTokens\":7,\"costUsd\":0.5}}\n\
        {\"type\":\"message-end\",\"input\":1,\"output\":2}\n\
    ";
    let usage = accumulate_usage(stream);
    assert_eq!(usage.usage_events, 2);
    assert_eq!(usage.input, Some(4));
    assert_eq!(usage.output, Some(6));
    assert_eq!(usage.reasoning, Some(7));
    assert_eq!(usage.cache_read, Some(5));
    assert_eq!(usage.cache_write, Some(6));
    assert!((usage.cost_estimate.unwrap() - 0.5).abs() < 1e-9);
}

// ============================================================================
// absorb
// ============================================================================

/// Folding a measured launch with an unmeasured one must not manufacture a
/// zero for a counter neither of them reported.
#[test]
fn absorbing_an_unmeasured_row_never_turns_an_unmeasured_counter_into_zero() {
    let mut total = TapUsage {
        input: Some(10),
        usage_events: 1,
        ..TapUsage::default()
    };
    total.absorb(&TapUsage::default());
    assert_eq!(total.input, Some(10));
    assert_eq!(total.output, None);
    assert_eq!(total.usage_events, 1);
}

// ============================================================================
// account_launch_log
// ============================================================================

#[test]
fn a_logs_usage_is_attributed_to_the_tap_its_own_launch_record_names() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":100,\"output\":20},\"cost\":0.25}"
        ),
    );
    let row = account_launch_log(&log, "sweep_id=s1").unwrap();
    assert_eq!(row.key(), "opencode:zai-metered@api_keys:zai");
    assert_eq!(row.usage.input, Some(100));
    assert!((row.usage.cost_estimate.unwrap() - 0.25).abs() < 1e-9);
}

/// An attributable launch whose stream reported nothing is still a row — "this
/// tap ran and we cannot see what it cost" must stay distinguishable from
/// "this tap ran for free", which is the whole point of a spend-governance read.
#[test]
fn an_attributable_launch_with_an_unreadable_stream_is_still_a_row() {
    let log = log_with("sweep_id=s1", &launch_line("pi", "env", None));
    let row = account_launch_log(&log, "sweep_id=s1").unwrap();
    assert_eq!(row.key(), "pi@env");
    assert!(!row.usage.is_measured());
}

#[test]
fn a_log_with_no_launch_record_or_a_missing_anchor_yields_nothing() {
    let log = log_with("sweep_id=s1", "{\"type\":\"step_finish\",\"tokens\":{\"input\":9}}");
    assert!(account_launch_log(&log, "sweep_id=s1").is_none());
    let log = log_with("sweep_id=other", &launch_line("pi", "env", None));
    assert!(account_launch_log(&log, "sweep_id=s1").is_none());
}

/// A per-issue log is reused across dispatches, so a previous run's usage must
/// never be attributed to this one — the same anchoring every other reader of
/// these logs applies.
#[test]
fn a_previous_dispatchs_usage_in_a_reused_log_is_not_attributed_to_this_one() {
    let log = format!(
        "{}{}",
        log_with(
            "sweep_id=old",
            &format!(
                "{}\n{}",
                launch_line("opencode:zai-metered", "pool", Some("zai")),
                "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000}}"
            )
        ),
        log_with(
            "sweep_id=new",
            &format!(
                "{}\n{}",
                launch_line("opencode:zai-metered", "pool", Some("zai")),
                "{\"type\":\"step_finish\",\"tokens\":{\"input\":7}}"
            )
        ),
    );
    let row = account_launch_log(&log, "sweep_id=new").unwrap();
    assert_eq!(row.usage.input, Some(7));
    assert_eq!(row.usage.usage_events, 1);
}

// ============================================================================
// fold_by_tap — the #8556 query
// ============================================================================

/// The question the issue says must become a query rather than a
/// reconstruction: how much went to the metered backstop vs. the subscriptions.
#[test]
fn folding_separates_the_metered_backstop_from_the_subscription_taps() {
    let metered = |input: u64, cost: f64| TapAccounting {
        tap: tap("opencode", Some("zai-metered"), "pool", Some("zai")),
        usage: TapUsage {
            input: Some(input),
            cost_estimate: Some(cost),
            usage_events: 1,
            ..TapUsage::default()
        },
    };
    let rows = vec![
        metered(100, 0.10),
        metered(50, 0.05),
        TapAccounting {
            tap: tap("pi", None, "none", None),
            usage: TapUsage {
                input: Some(1_000),
                usage_events: 1,
                ..TapUsage::default()
            },
        },
    ];
    let folded = fold_by_tap(&rows);
    assert_eq!(
        folded.keys().cloned().collect::<Vec<_>>(),
        vec![
            "opencode:zai-metered@api_keys:zai".to_string(),
            "pi@harness-own".to_string(),
        ]
    );
    let backstop = &folded["opencode:zai-metered@api_keys:zai"];
    assert_eq!(backstop.input, Some(150));
    assert_eq!(backstop.usage_events, 2);
    assert!((backstop.cost_estimate.unwrap() - 0.15).abs() < 1e-9);
    // The flat-rate tap's estimate is absent, not zero — a flat-rate tap's
    // cost estimate is not a charge at all (`runtime-model-trials.md`).
    assert_eq!(folded["pi@harness-own"].cost_estimate, None);
}
