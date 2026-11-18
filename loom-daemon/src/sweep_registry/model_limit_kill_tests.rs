//! In-flight model fallback (#13905): the banner classifier, the rung step,
//! the end-of-step memo, and the quarantine-tally exemption it mirrors.

use super::*;
use crate::sweep_registry::test_support::write_config;
use serial_test::serial;
use std::fs;

const ON: &str = r#"{"autonomous": {"modelFallback": {"enabled": true}}}"#;

fn chain(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|s| (*s).to_string()).collect()
}

/// A workspace plus a step log whose tail is `text`.
fn fixture(config: &str, text: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    write_config(tmp.path(), config);
    let log = tmp.path().join("step.log");
    fs::write(&log, text).unwrap();
    (tmp, log)
}

const FABLE_DEATH: &str =
    "working...\nYou've hit your Fable limit. Run /usage-credits to continue.\n";
const OPUS_DEATH: &str = "working...\nYou've reached your Opus 5 limit.\n";
const SONNET_DEATH: &str = "working...\nYou've hit your Sonnet limit.\n";

// ---- the classifier ------------------------------------------------------

#[test]
fn a_banner_naming_a_model_names_its_class() {
    for (text, class) in [
        ("You've hit your Fable limit", "fable"),
        ("You've reached your Fable 5 limit. Run /usage-credits", "fable"),
        ("hit your opus limit", "opus"),
        ("Claude: you have reached your Sonnet limit.", "sonnet"),
    ] {
        assert_eq!(killed_model_class(text).as_deref(), Some(class), "{text}");
    }
}

#[test]
fn a_banner_naming_no_model_is_account_wide_and_not_a_model_kill() {
    for text in [
        "You've hit your limit",
        "You've hit your weekly limit",
        "Claude: hit your session limit",
        "monthly usage limit",
        "reached your goal well before the team agreed on any spending limit",
        "Error: you have reached your concurrent session limit",
        "all quiet",
        "",
    ] {
        assert_eq!(killed_model_class(text), None, "{text}");
    }
}

#[test]
fn a_concurrency_line_does_not_hide_a_model_banner_on_another_line() {
    let tail = "Error: you have reached your concurrent session limit\n\
                You've reached your Fable 5 limit.";
    assert_eq!(killed_model_class(tail).as_deref(), Some("fable"));
}

// ---- the rung ------------------------------------------------------------

#[test]
fn the_next_rung_is_strictly_below_and_stops_at_the_last() {
    let c = chain(&["fable", "opus", "sonnet"]);
    assert_eq!(next_rung(&c, "fable").as_deref(), Some("opus"));
    assert_eq!(next_rung(&c, "opus").as_deref(), Some("sonnet"));
    assert_eq!(next_rung(&c, "sonnet"), None, "the last rung has nowhere to go");
    assert_eq!(next_rung(&c, "haiku"), None, "off the chain has no provable below");
}

#[test]
fn the_next_rung_skips_unrecognized_and_same_class_entries() {
    let c = chain(&["fable", "mystery-model", "claude-fable-5", "opus"]);
    assert_eq!(next_rung(&c, "fable").as_deref(), Some("opus"));
}

// ---- the step end and the redraw -----------------------------------------

#[test]
#[serial(loom_config_env)]
fn a_fable_kill_is_uncharged_and_the_redraw_launches_opus() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    let (model, decision) = step_down(tmp.path(), 7, "builder", "fable", "fable").unwrap();
    assert_eq!(model, "opus");
    assert_eq!(decision.model, "opus");
    assert_eq!(decision.from, "fable");
    assert_eq!(decision.reason, REASON_MODEL_LIMIT_KILL);
}

#[test]
#[serial(loom_config_env)]
fn a_second_kill_at_opus_steps_to_sonnet_never_looping_at_opus() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    // The redraw ran on opus and was killed there too.
    fs::write(&log, OPUS_DEATH).unwrap();
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    let (model, decision) = step_down(tmp.path(), 7, "builder", "fable", "fable").unwrap();
    assert_eq!(model, "sonnet");
    assert_eq!(decision.from, "fable");
}

#[test]
#[serial(loom_config_env)]
fn a_kill_at_the_last_rung_is_handled_as_today() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    fs::write(&log, SONNET_DEATH).unwrap();
    assert!(
        !observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died),
        "nothing below sonnet: charged as today"
    );
    assert!(
        step_down(tmp.path(), 7, "builder", "fable", "fable").is_none(),
        "and the stale fable step-down is spent, so there is no loop"
    );
}

#[test]
#[serial(loom_config_env)]
fn a_step_that_ends_any_other_way_spends_the_pending_step_down() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    fs::write(&log, "all done\n").unwrap();
    assert!(!observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Clean));
    assert!(step_down(tmp.path(), 7, "builder", "fable", "fable").is_none());
}

#[test]
#[serial(loom_config_env)]
fn a_clean_end_never_reads_as_a_kill_even_with_the_banner_in_its_log() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(!observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Clean));
    assert!(step_down(tmp.path(), 7, "builder", "fable", "fable").is_none());
}

#[test]
#[serial(loom_config_env)]
fn a_banner_quoted_far_above_the_tail_is_not_a_kill() {
    // A step that read an issue quoting the banner, then died on something
    // else: the phrase is outside the trailing window.
    let mut text = String::from("issue says: You've hit your Fable limit\n");
    for i in 0..BANNER_TAIL_LINES + 5 {
        text.push_str(&format!("line {i}\n"));
    }
    let (tmp, log) = fixture(ON, &text);
    assert!(!observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
}

#[test]
#[serial(loom_config_env)]
fn the_memo_is_per_item_and_role() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    assert!(step_down(tmp.path(), 8, "builder", "fable", "fable").is_none());
    assert!(step_down(tmp.path(), 7, "judge", "fable", "fable").is_none());
}

#[test]
#[serial(loom_config_env)]
fn a_launch_already_below_the_killed_class_stands() {
    let (tmp, log) = fixture(ON, FABLE_DEATH);
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    // Selection-time fallback already took the launch to sonnet: never climb
    // back up to the opus rung.
    assert!(step_down(tmp.path(), 7, "builder", "sonnet", "fable").is_none());
    // A model off the chain has no provable relation to the killed class.
    assert!(step_down(tmp.path(), 7, "builder", "haiku", "haiku").is_none());
}

#[test]
#[serial(loom_config_env)]
fn disabled_or_absent_fallback_reproduces_today() {
    for config in [
        r#"{"autonomous": {"modelFallback": {"enabled": false}}}"#,
        "{}",
    ] {
        let (tmp, log) = fixture(config, FABLE_DEATH);
        assert!(!observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
        assert!(step_down(tmp.path(), 7, "builder", "fable", "fable").is_none());
    }
}

#[test]
#[serial(loom_config_env)]
fn a_custom_chain_is_honoured() {
    let (tmp, log) = fixture(
        r#"{"autonomous": {"modelFallback": {"enabled": true, "chain": ["fable", "sonnet"]}}}"#,
        FABLE_DEATH,
    );
    assert!(observe_step_end(tmp.path(), 7, "builder", &log, EndKind::Died));
    let (model, _) = step_down(tmp.path(), 7, "builder", "fable", "fable").unwrap();
    assert_eq!(model, "sonnet");
}

// ---- the quarantine-tally exemption (AC2) --------------------------------

/// The issue sweep path's insta-crash tally already exempts the exact
/// `hit your Fable limit` text (the Curator's trace, #13905 AC2): pinned so a
/// future signature edit cannot silently charge the issue.
#[test]
fn the_exact_fable_banner_classifies_as_account_exhaustion_not_an_issue_crash() {
    assert!(
        super::super::crash_signals::classify_account_exhaustion(FABLE_DEATH).is_some(),
        "an insta-crash on this banner must not be charged to the issue's tally"
    );
}

// ---- the legacy issue-sweep path (#14443) --------------------------------

mod issue_sweep {
    use super::*;
    use crate::sweep_registry::test_support::{fixture_registry, insert_dead_running_with_log};
    use crate::sweep_registry::{DispatchModel, SweepRegistry};
    use crate::types::SweepKind;

    const ISSUE: u32 = 14_443;
    const CONFIG: &str =
        r#"{"autonomous": {"model": "fable", "modelFallback": {"enabled": true}}}"#;

    fn registry(config: &str) -> (tempfile::TempDir, SweepRegistry) {
        let tmp = tempfile::tempdir().unwrap();
        let (registry, _record) = fixture_registry(tmp.path());
        write_config(tmp.path(), config);
        (tmp, registry)
    }

    /// An issue sweep dies with `log` as its output and the reaper sees it.
    fn die(registry: &mut SweepRegistry, seq: u32, log: &str) {
        let id = insert_dead_running_with_log(registry, ISSUE, seq, "unknown", log);
        assert!(registry.reap_once() >= 1);
        assert!(registry.get(&id).is_some());
    }

    /// The model the issue's next fresh dispatch launches with.
    fn next_launch(registry: &SweepRegistry) -> Option<String> {
        DispatchModel::Autonomous.resolve_for_runtime(
            &registry.config,
            &SweepKind::Issue(ISSUE),
            Some("claude"),
        )
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_killed_issue_sweep_is_relaunched_one_rung_down_and_never_loops() {
        let (_tmp, mut registry) = registry(CONFIG);
        assert_eq!(next_launch(&registry).as_deref(), Some("fable"), "no kill yet");

        die(&mut registry, 0, FABLE_DEATH);
        assert_eq!(next_launch(&registry).as_deref(), Some("opus"));

        // The relaunch on opus was killed too: one rung further, not opus again.
        die(&mut registry, 1, OPUS_DEATH);
        assert_eq!(next_launch(&registry).as_deref(), Some("sonnet"));

        // Killed at the last rung: nothing below, so no memo and the death is
        // handled as today; the next launch goes back to the configured model.
        die(&mut registry, 2, SONNET_DEATH);
        assert_eq!(next_launch(&registry).as_deref(), Some("fable"));
    }

    #[test]
    #[serial(loom_config_env)]
    fn the_stepped_down_launch_carries_the_model_limit_kill_decision() {
        let (tmp, mut registry) = registry(CONFIG);
        die(&mut registry, 0, FABLE_DEATH);
        let (model, decision) = step_down(tmp.path(), ISSUE, ISSUE_SWEEP_ROLE, "fable", "fable")
            .expect("the reaper recorded the kill");
        assert_eq!(model, "opus");
        assert_eq!(decision.reason, REASON_MODEL_LIMIT_KILL);
    }

    #[test]
    #[serial(loom_config_env)]
    fn the_reaper_resume_redispatch_also_steps_down() {
        // `Resolved(None)` is what the reaper's resume passes: no `--model`
        // today. With a kill pending it resolves a model and steps it down.
        let (_tmp, mut registry) = registry(CONFIG);
        let resume = |r: &SweepRegistry| {
            DispatchModel::Resolved(None).resolve_for_runtime(
                &r.config,
                &SweepKind::Issue(ISSUE),
                Some("claude"),
            )
        };
        assert_eq!(resume(&registry), None, "no kill pending: the literal None stands");
        die(&mut registry, 0, FABLE_DEATH);
        assert_eq!(resume(&registry).as_deref(), Some("opus"));
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_literal_model_pin_is_never_overridden_by_a_pending_kill() {
        let (_tmp, mut registry) = registry(CONFIG);
        die(&mut registry, 0, FABLE_DEATH);
        assert_eq!(
            DispatchModel::Resolved(Some("fable"))
                .resolve_for_runtime(&registry.config, &SweepKind::Issue(ISSUE), Some("claude"))
                .as_deref(),
            Some("fable")
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn absent_or_disabled_fallback_leaves_the_launch_alone() {
        for config in [
            r#"{"autonomous": {"model": "fable"}}"#,
            r#"{"autonomous": {"model": "fable", "modelFallback": {"enabled": false}}}"#,
        ] {
            let (tmp, mut registry) = registry(config);
            die(&mut registry, 0, FABLE_DEATH);
            assert_eq!(next_launch(&registry).as_deref(), Some("fable"), "{config}");
            assert!(!step_down_pending(tmp.path(), ISSUE, ISSUE_SWEEP_ROLE), "{config}");
        }
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_banner_naming_no_model_does_not_step_down() {
        let (tmp, mut registry) = registry(CONFIG);
        die(&mut registry, 0, "working...\nYou've hit your weekly limit.\n");
        assert!(!step_down_pending(tmp.path(), ISSUE, ISSUE_SWEEP_ROLE));
        assert_eq!(next_launch(&registry).as_deref(), Some("fable"));
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_death_without_the_banner_spends_a_pending_step_down() {
        let (tmp, mut registry) = registry(CONFIG);
        die(&mut registry, 0, FABLE_DEATH);
        assert!(step_down_pending(tmp.path(), ISSUE, ISSUE_SWEEP_ROLE));
        die(&mut registry, 1, "working...\nsegfault\n");
        assert!(!step_down_pending(tmp.path(), ISSUE, ISSUE_SWEEP_ROLE));
        assert_eq!(next_launch(&registry).as_deref(), Some("fable"));
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_step_and_an_issue_sweep_on_one_item_do_not_share_a_memo() {
        let (tmp, mut registry) = registry(CONFIG);
        die(&mut registry, 0, FABLE_DEATH);
        for role in ["builder", "judge", "doctor"] {
            assert!(!step_down_pending(tmp.path(), ISSUE, role), "{role}");
        }
    }
}
