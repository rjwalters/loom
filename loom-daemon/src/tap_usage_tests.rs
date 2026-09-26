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

/// One Pi 0.85.1 message exactly as its event stream carries it (Issue #8934):
/// counters nested under `message.usage`, `cost` an **object** with a rollup
/// `total`, and `reasoning` a subset of `output` (`input + output + cacheRead +
/// cacheWrite == totalTokens`, with `reasoning` counted in neither). Shape from
/// `pi_usage`'s "Schema provenance" section, which read it off the shipped
/// `@earendil-works/pi-coding-agent` / `pi-ai` packages.
fn pi_message(role: &str) -> Value {
    serde_json::json!({
        "role": role,
        "provider": "anthropic",
        "model": "claude-sonnet-4-5",
        "usage": {
            "input": 1000,
            "output": 200,
            "cacheRead": 4000,
            "cacheWrite": 300,
            "reasoning": 5,
            "totalTokens": 5500,
            "cost": {
                "input": 0.1,
                "output": 0.2,
                "cacheRead": 0.0,
                "cacheWrite": 0.0,
                "total": 0.3,
            },
        },
        "timestamp": 1_790_330_401_000_i64,
    })
}

/// A Pi stream event of `kind` carrying [`pi_message`] — `message_end` for the
/// real reading, `agent_end`/`entry_appended` for the repeats that must not be
/// counted.
fn pi_event(kind: &str, role: &str) -> String {
    serde_json::json!({ "type": kind, "message": pi_message(role) }).to_string()
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

/// Pi 0.85.1's **real** `message_end` shape (Issue #8934): the counters live
/// under `message.usage`, not on the event. Before the fix this stream read as
/// completely unmeasured, so every Pi launch's tap spend was invisible.
///
/// `runtime-model-trials.md`: "Pi usage appears on assistant `message_end`
/// events; count each once, not again inside `agent_end`." `entry_appended`
/// re-emits every persisted assistant message, so it is excluded for the same
/// reason — counting either repeat would double every Pi run.
#[test]
fn pi_message_end_usage_is_counted_once_and_repeat_events_are_ignored() {
    let stream = format!(
        "{}\n{}\n{}\n",
        pi_event("message_end", "assistant"),
        pi_event("agent_end", "assistant"),
        pi_event("entry_appended", "assistant"),
    );
    let usage = accumulate_usage(&stream);
    assert_eq!(usage.usage_events, 1, "neither agent_end nor entry_appended may be folded in");
    assert_eq!(usage.input, Some(1000));
    assert_eq!(usage.output, Some(200));
    assert_eq!(usage.cache_read, Some(4000));
    assert_eq!(usage.cache_write, Some(300));
    assert!(usage.is_measured());
    // `usage.cost` is an object, so the estimate is its rollup `total`.
    assert!((usage.cost_estimate.unwrap() - 0.3).abs() < 1e-9);
    // Pi's `reasoning` is already inside `output`, and `total_tokens` sums every
    // counter it is given — so it is deliberately not recorded here.
    assert_eq!(
        usage.reasoning, None,
        "Pi's reasoning is a subset of output, never a counter of its own"
    );
    assert_eq!(
        usage.total_tokens(),
        Some(5500),
        "matches Pi's own totalTokens: reasoning is not double-counted"
    );
}

/// The `message_end` of a `toolResult` message carries the usage of LLM work
/// done **inside** a tool. That is real spend against the tap that paid for it,
/// so this module counts it — unlike `pi_usage`, which excludes it because such
/// a reading names no model and that reader keys by model. Keyed by tap, there
/// is nothing to guess, and dropping it would be a knowable undercount.
#[test]
fn a_tool_results_nested_usage_is_charged_to_the_tap_that_paid_for_it() {
    let usage = accumulate_usage(&format!("{}\n", pi_event("message_end", "toolResult")));
    assert_eq!(usage.usage_events, 1);
    assert_eq!(usage.input, Some(1000));
    assert_eq!(usage.output, Some(200));
    assert!((usage.cost_estimate.unwrap() - 0.3).abs() < 1e-9);
}

/// `cost` is a bare number for OpenCode and an object for Pi, so both spellings
/// are read — and an object with no rollup total is unmeasured, never zero.
#[test]
fn a_cost_object_is_read_from_its_rollup_total_and_a_bare_number_still_works() {
    let nested = accumulate_usage(
        "{\"type\":\"message_end\",\"message\":{\"usage\":{\"input\":1,\"cost\":{\"input\":0.1,\"total\":0.4}}}}\n",
    );
    assert!((nested.cost_estimate.unwrap() - 0.4).abs() < 1e-9);

    let flat =
        accumulate_usage("{\"type\":\"step_finish\",\"tokens\":{\"input\":1},\"cost\":0.02}\n");
    assert!((flat.cost_estimate.unwrap() - 0.02).abs() < 1e-9);

    let no_total = accumulate_usage(
        "{\"type\":\"message_end\",\"message\":{\"usage\":{\"input\":1,\"cost\":{\"input\":0.1}}}}\n",
    );
    assert_eq!(no_total.cost_estimate, None);
    assert_eq!(no_total.input, Some(1), "the counters are still read");
}

/// The flat `message_end` spelling other harnesses use (and Pi's `agent_end`
/// sibling fields) keeps working: adding the `message.usage` scope widened the
/// search rather than replacing it.
#[test]
fn a_flat_message_end_usage_object_is_still_read() {
    let usage = accumulate_usage(
        "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":50,\"output_tokens\":7,\"reasoning_tokens\":3}}\n",
    );
    assert_eq!(usage.usage_events, 1);
    assert_eq!(usage.input, Some(50));
    assert_eq!(usage.output, Some(7));
    assert_eq!(
        usage.reasoning,
        Some(3),
        "only a reading out of Pi's message.usage suppresses reasoning"
    );
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

/// Issue #8934 end-to-end: a real Pi launch's region is **measured** and
/// attributed to the pi tap. Before the fix this was the row the test above
/// pins — attributable but unmeasured — for a launch that had in fact reported
/// every counter it has.
#[test]
fn a_real_pi_launchs_region_is_measured_and_attributed_to_the_pi_tap() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}",
            launch_line("pi", "env", None),
            pi_event("message_end", "assistant"),
            pi_event("agent_end", "assistant"),
        ),
    );
    let row = account_launch_log(&log, "sweep_id=s1").unwrap();
    assert_eq!(row.key(), "pi@env");
    assert!(row.usage.is_measured(), "a real Pi launch is not unmeasured");
    assert_eq!(row.usage.usage_events, 1);
    assert_eq!(row.usage.input, Some(1000));
    assert_eq!(row.usage.output, Some(200));
    assert_eq!(row.usage.cache_read, Some(4000));
    assert_eq!(row.usage.cache_write, Some(300));
    assert_eq!(row.usage.total_tokens(), Some(5500));
    assert!((row.usage.cost_estimate.unwrap() - 0.3).abs() < 1e-9);
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

/// Issue #8633: a region may hold more than one launch record (a re-dispatch
/// inside one sweep, a containment re-exec, a phase pinned to its own tap by
/// `rolePreference`/`LOOM_RUNTIME_<ROLE>`), and the last record's tap must not
/// be handed the whole region's usage.
///
/// Here the metered launch is the **earlier** one, which is why folding a
/// region onto its last tap is not safely conservative: the untouched
/// behaviour charged 9007 input tokens of metered spend to a flat-rate
/// subscription tap.
#[test]
fn a_multi_record_regions_usage_is_sliced_per_record_not_folded_onto_the_last() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000,\"output\":800},\"cost\":0.25}",
            launch_line("codex", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}"
        ),
    );

    // The one-row contract still names the LAST launch — but with only its own
    // usage.
    let row = account_launch_log(&log, "sweep_id=s1").unwrap();
    assert_eq!(row.key(), "codex@env");
    assert_eq!(
        row.usage.input,
        Some(7),
        "the earlier launch's 9000 input tokens belong to the metered tap, not to codex"
    );
    assert_eq!(row.usage.output, None);
    assert_eq!(row.usage.cost_estimate, None, "a metered cost estimate must not follow the tap");
    assert_eq!(row.usage.usage_events, 1);

    // The lossless read: both launches, each with its own block's usage.
    let rows = account_launch_logs(&log, "sweep_id=s1");
    assert_eq!(
        rows.iter().map(TapAccounting::key).collect::<Vec<_>>(),
        vec![
            "opencode:zai-metered@api_keys:zai".to_string(),
            "codex@env".to_string()
        ],
        "rows come back in log order"
    );
    assert_eq!(rows[0].usage.input, Some(9000));
    assert_eq!(rows[0].usage.output, Some(800));
    assert!((rows[0].usage.cost_estimate.unwrap() - 0.25).abs() < 1e-9);

    let folded = fold_by_tap(&rows);
    assert_eq!(folded["opencode:zai-metered@api_keys:zai"].input, Some(9000));
    assert_eq!(folded["codex@env"].input, Some(7));
}

/// A single-record region — the overwhelmingly common shape — is unchanged by
/// the slicing: one row, all of the region's usage, and `account_launch_logs`
/// agrees with `account_launch_log`.
#[test]
fn a_single_record_region_is_unaffected_by_per_record_slicing() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":100,\"output\":20},\"cost\":0.25}",
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":50}}"
        ),
    );
    let row = account_launch_log(&log, "sweep_id=s1").unwrap();
    assert_eq!(row.usage.input, Some(150));
    assert_eq!(row.usage.output, Some(20));
    assert_eq!(row.usage.usage_events, 2);
    assert_eq!(account_launch_logs(&log, "sweep_id=s1"), vec![row]);
}

/// Usage ahead of a region's first launch record had no tap announced yet, so
/// it is charged to nobody rather than to the launch that started afterwards.
#[test]
fn usage_before_the_first_launch_record_is_charged_to_nobody() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}",
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000}}",
            launch_line("pi", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}"
        ),
    );
    let row = account_launch_log(&log, "sweep_id=s1").unwrap();
    assert_eq!(row.key(), "pi@env");
    assert_eq!(row.usage.input, Some(7));
    assert_eq!(account_launch_logs(&log, "sweep_id=s1").len(), 1);
}

/// The marker is matched line-anchored, exactly as
/// `api_keys_pool::ingest::parse_launch_record` matches it, so an agent
/// transcript echoing `# LOOM_LAUNCH ` mid-line opens no phantom block and
/// cannot suppress the real record's attribution.
#[test]
fn a_mid_line_mention_of_the_marker_opens_no_block() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}",
            launch_line("pi", "env", None),
            "agent transcript: the record is written as # LOOM_LAUNCH {\"schema\":1} by the child",
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}"
        ),
    );
    let rows = account_launch_logs(&log, "sweep_id=s1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key(), "pi@env");
    assert_eq!(rows[0].usage.input, Some(7));
}

/// An unattributable record (no runtime to key a tap on) contributes no row,
/// and its usage is dropped rather than charged to a neighbouring tap.
#[test]
fn an_unattributable_record_absorbs_its_own_usage_and_yields_no_row() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("pi", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}",
            r#"# LOOM_LAUNCH {"schema":1,"credentialSource":"env"}"#,
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000}}"
        ),
    );
    // The last record is unattributable, so the one-row read declines — the
    // same "no opinion" it has always returned for that record.
    assert!(account_launch_log(&log, "sweep_id=s1").is_none());
    let rows = account_launch_logs(&log, "sweep_id=s1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key(), "pi@env");
    assert_eq!(
        rows[0].usage.input,
        Some(7),
        "the unattributable record's 9000 tokens must not land on the pi launch"
    );
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

// ============================================================================
// account_region_by_tap — the journal-shaped read (Issue #8659)
// ============================================================================

/// The multi-tap region #8659 names: after #8633 the earlier launch was no
/// longer misattributed, but a one-row reader did not record it at all. Both
/// taps now come back, each with its own share and neither merged into the
/// other.
#[test]
fn a_multi_tap_region_reports_every_taps_share_with_the_outcomes_tap_first() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000,\"output\":800},\"cost\":0.25}",
            launch_line("codex", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}"
        ),
    );
    let region = account_region_by_tap(&log, "sweep_id=s1");

    // The outcome's own row is still the LAST record — unchanged from
    // `account_launch_log`, and with only its own tap's usage.
    let outcome = region.outcome().expect("the last record is attributable");
    assert_eq!(outcome.key(), "codex@env");
    assert_eq!(outcome.usage.input, Some(7));
    assert_eq!(outcome.usage.cost_estimate, None);

    // …and the earlier metered launch is now recorded rather than dropped.
    assert_eq!(
        region
            .breakdown()
            .iter()
            .map(TapAccounting::key)
            .collect::<Vec<_>>(),
        vec![
            "codex@env".to_string(),
            "opencode:zai-metered@api_keys:zai".to_string()
        ],
        "outcome's tap first, then the remaining taps in order of first appearance"
    );
    let metered = &region.breakdown()[1];
    assert_eq!(metered.usage.input, Some(9000));
    assert_eq!(metered.usage.output, Some(800));
    assert!((metered.usage.cost_estimate.unwrap() - 0.25).abs() < 1e-9);
}

/// A re-dispatch or containment re-exec re-announces the **same** tap, so its
/// blocks belong in one row: the outcome row carries the whole region and a
/// one-row reader loses nothing — the concrete gain over reading only the
/// region's last block.
#[test]
fn a_same_tap_multi_record_region_folds_into_one_row_that_covers_it_all() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000},\"cost\":0.25}",
            launch_line("opencode:zai-metered", "pool", Some("zai")),
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":7}}"
        ),
    );
    let region = account_region_by_tap(&log, "sweep_id=s1");
    let outcome = region.outcome().expect("attributable");
    assert_eq!(outcome.key(), "opencode:zai-metered@api_keys:zai");
    assert_eq!(
        outcome.usage.input,
        Some(9007),
        "both blocks are the same tap's spend, so one row must carry both"
    );
    assert_eq!(outcome.usage.usage_events, 2);
    assert!((outcome.usage.cost_estimate.unwrap() - 0.25).abs() < 1e-9);
    assert!(
        region.breakdown().is_empty(),
        "one tap ⇒ the outcome row is the whole region, so there is nothing to break out"
    );
}

/// The overwhelmingly common shape stays exactly what it was: one row, the
/// whole region's usage, and nothing to break out.
#[test]
fn a_single_record_region_has_an_outcome_row_and_an_empty_breakdown() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}",
            launch_line("pi", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}"
        ),
    );
    let region = account_region_by_tap(&log, "sweep_id=s1");
    assert_eq!(
        region.outcome().map(TapAccounting::key),
        account_launch_log(&log, "sweep_id=s1").map(|row| row.key()),
        "the one-row contract is unchanged for a single-record region"
    );
    assert_eq!(region.outcome().unwrap().usage.input, Some(7));
    assert!(region.breakdown().is_empty());
}

/// An unattributable **last** record has no tap to name, so the outcome row
/// declines exactly as `account_launch_log` does — an earlier launch is never
/// promoted into "which tap did this sweep run on". Its measured usage is not
/// lost either: the breakdown carries it, which is the whole point of #8659.
#[test]
fn an_unattributable_last_record_declines_the_outcome_row_but_keeps_the_breakdown() {
    let log = log_with(
        "sweep_id=s1",
        &format!(
            "{}\n{}\n{}\n{}",
            launch_line("pi", "env", None),
            "{\"type\":\"message_end\",\"usage\":{\"input_tokens\":7}}",
            r#"# LOOM_LAUNCH {"schema":1,"credentialSource":"env"}"#,
            "{\"type\":\"step_finish\",\"tokens\":{\"input\":9000}}"
        ),
    );
    let region = account_region_by_tap(&log, "sweep_id=s1");
    assert!(account_launch_log(&log, "sweep_id=s1").is_none());
    assert!(
        region.outcome().is_none(),
        "no attributable last record ⇒ no opinion about the outcome's tap"
    );
    assert_eq!(
        region
            .breakdown()
            .iter()
            .map(TapAccounting::key)
            .collect::<Vec<_>>(),
        vec!["pi@env".to_string()]
    );
    assert_eq!(
        region.breakdown()[0].usage.input,
        Some(7),
        "the unattributable record's 9000 tokens stay charged to nobody"
    );
}

/// A region with no launch record at all is "no opinion" in both halves — the
/// Claude/legacy-adapter shape every record on the fleet has today.
#[test]
fn a_region_with_no_launch_record_is_empty_in_both_halves() {
    let region = account_region_by_tap(&log_with("sweep_id=s1", "clean run"), "sweep_id=s1");
    assert!(region.outcome().is_none());
    assert!(region.breakdown().is_empty());
    assert!(region.per_tap.is_empty());
    // A missing anchor is likewise empty, never a previous dispatch's region.
    assert!(account_region_by_tap(&log_with("sweep_id=s1", "x"), "sweep_id=absent")
        .per_tap
        .is_empty());
}
