//! Tests for the retry-classifier CLI boundary (#8037).

use super::*;

fn opts_with(classification: Option<&str>, transient: bool, exit_code: i32) -> Opts {
    Opts {
        exit_code,
        classification: classification.map(str::to_string),
        classification_is_transient: transient,
        ..Default::default()
    }
}

#[test]
fn true_is_zero_and_false_is_one() {
    // The shell branches on the exit status alone, so the polarity is contract:
    // 0 = the predicate holds. Inverting it would turn every rotation decision
    // into its opposite at once.
    let exhausted = opts_with(Some("TOKEN_EXHAUSTED"), true, 1);
    assert_eq!(run(Sub::AccountExhaustion, &exhausted, ""), 0);
    let recoverable = opts_with(Some("RECOVERABLE"), true, 1);
    assert_eq!(run(Sub::AccountExhaustion, &recoverable, ""), 1);
}

#[test]
fn a_usage_error_does_not_reuse_the_false_exit_code() {
    // Prevents: an old binary that does not know this subcommand — clap exits 2
    // — being read as a confident "no, not exhausted". The wrapper maps
    // anything above 1 to its fail-safe instead.
    assert_ne!(EX_USAGE, 0);
    assert_ne!(EX_USAGE, 1);
}

#[test]
fn transient_prints_the_category_it_acted_on() {
    // The wrapper caches this as _LAST_ERROR_CLASSIFICATION and logs it next to
    // the decision (#4501). For the wrapper's own sentinel that is
    // RATE_LIMIT_ABORT, not the RECOVERABLE `classify_error` would report.
    let opts = opts_with(Some("RECOVERABLE"), true, 1);
    // Verdict only — stdout is asserted end-to-end by
    // test-claude-wrapper-retry.sh, which reads it through the shell function.
    assert_eq!(run(Sub::Transient, &opts, "RATE_LIMIT_ABORT"), 1);
    assert_eq!(run(Sub::Transient, &opts, "connection reset by peer"), 0);
}

#[test]
fn a_missing_classification_selects_the_degraded_path() {
    // `--classification` absent is how the shell reports "lib/classify-error.sh
    // was not sourced". It must not be spelled as an empty string, which would
    // be an unknown CATEGORY rather than an unavailable classifier.
    let degraded = opts_with(None, false, 1);
    assert_eq!(run(Sub::Transient, &degraded, "anything at all"), 0);
    assert_eq!(run(Sub::AccountExhaustion, &degraded, "You have hit your weekly limit"), 0);

    let empty_category = opts_with(Some(""), false, 1);
    assert_eq!(run(Sub::Transient, &empty_category, "anything at all"), 1);
}

#[test]
fn wait_time_always_answers_zero() {
    // It is not a predicate: the caller needs a number, and a non-zero exit
    // here would read as a failed classification.
    let opts = Opts {
        attempt: 3,
        initial_wait: 60,
        multiplier: 2,
        max_wait: 1800,
        ..Default::default()
    };
    assert_eq!(run(Sub::WaitTime, &opts, ""), 0);
}

#[test]
fn mcp_error_reads_output_and_not_the_exit_code() {
    // The one predicate with no exit-code conjunction, asserted at the boundary
    // too: a zero exit whose output names an MCP failure is still an MCP error.
    let zero_exit = opts_with(None, false, 0);
    assert_eq!(run(Sub::McpError, &zero_exit, "MCP server failed"), 0);
    assert_eq!(run(Sub::McpError, &zero_exit, "connection reset by peer"), 1);
}

#[test]
fn model_class_answers_with_the_same_polarity_as_every_other_predicate() {
    // The caller splices stdout into `tokens mark-bad --reason` only when the
    // status says to, so 0 must mean "a marker was printed". Reusing 1 for
    // "account-wide" (rather than, say, exiting 0 with empty stdout) is what
    // lets the shell's `&& printf` idiom stay a one-liner.
    let scoped = Opts {
        classification: Some("MODEL_CREDITS_EXHAUSTED".to_string()),
        model: "claude-opus-5".to_string(),
        ..Default::default()
    };
    assert_eq!(run(Sub::ModelClass, &scoped, "You're out of usage credits."), 0);

    let account_wide = Opts {
        model: "claude-opus-5".to_string(),
        ..Default::default()
    };
    assert_eq!(run(Sub::ModelClass, &account_wide, "You've reached your weekly limit."), 1);
}

#[test]
fn an_empty_model_is_an_answer_not_a_missing_value() {
    // Unlike `--classification`, whose ABSENCE selects the degraded code path,
    // an empty `--model` selects a different VERDICT on the same path: the
    // spawn took the session default, so the mark stays account-wide. Spelling
    // it as an Option would invite a caller to omit it and get the other one.
    let no_model = Opts {
        classification: Some("MODEL_CREDITS_EXHAUSTED".to_string()),
        ..Default::default()
    };
    assert_eq!(run(Sub::ModelClass, &no_model, "You're out of usage credits."), 1);
}
