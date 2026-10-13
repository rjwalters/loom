//! A sanitized, deterministic **mixed sweep** (Issue #11360), shared by the
//! parser-level tests (`tap_usage_tests.rs`) and the end-to-end journal tests
//! (`sweep_registry/outcome_journal/tap_usage_tests.rs`), so both levels check
//! the same hand-computed totals.
//!
//! The shape: a parent sweep whose orchestrator (and so its in-process
//! Curator and Judge) ran on Codex or Claude, plus Builder and Doctor phases
//! launched out of process by `loom-daemon worker run` on OpenCode/zai-flash,
//! each writing a phase log of its own. Every line is the shape the real
//! writers produce (`worker_spawn::run`'s launch record, `worker run`'s phase
//! header, OpenCode's `step_finish`), but no value is real: account names are
//! unmistakably fake and no record carries key material at all — the launch
//! record never does (`launch_record`'s module doc).
//!
//! Hand-computed per-tap totals for [`mixed_sweep`]:
//!
//! | tap key | input | output | cache read | cost est. | events |
//! |---|---|---|---|---|---|
//! | `codex@env` (parent: Curator 1000/100 + Judge 2000/200) | 3000 | 300 | — | — | 2 |
//! | `opencode:zai-flash@api_keys:zai-coding-plan` (Builder 5000/500/20000/0.05 + 3000/300/—/0.03, Doctor 700/70/—/0.007) | 8700 | 870 | 20000 | 0.087 | 3 |
//!
//! Cache write and reasoning are reported by nothing, so they stay `None`.

#![allow(clippy::unwrap_used)]

/// The fixture issue every log below names.
pub(crate) const ISSUE: u32 = 4242;

/// Account name on the GLM launch records. Fake by construction.
pub(crate) const FAKE_GLM_ACCOUNT: &str = "FAKE-fixture-account-not-a-credential";

/// The tap key the Builder and Doctor phases are charged to.
pub(crate) const GLM_KEY: &str = "opencode:zai-flash@api_keys:zai-coding-plan";

/// The tap key a Codex parent orchestrator is charged to.
pub(crate) const CODEX_KEY: &str = "codex@env";

/// A `# LOOM_LAUNCH` record for the OpenCode/zai-flash phase tap, with the full
/// field set `worker_spawn::run` writes.
pub(crate) fn glm_launch() -> String {
    glm_launch_with_model("glm-fixture-flash")
}

/// [`glm_launch`] naming a different model — the relaunch shape the model-in
/// the-fold-identity test needs (Judge feedback on #11501).
pub(crate) fn glm_launch_with_model(model: &str) -> String {
    format!(
        "# LOOM_LAUNCH {}",
        serde_json::json!({
            "schema": 1,
            "runtime": "opencode",
            "tap": "opencode:zai-flash",
            "provider": "zai-coding-plan",
            "model": model,
            "profile": "zai-flash",
            "effort": serde_json::Value::Null,
            "credentialSource": "pool",
            "credentialProvider": "zai-coding-plan",
            "credentialAccount": FAKE_GLM_ACCOUNT,
            "usage": "native-json-events",
            "billing": "not-measured",
        })
    )
}

/// A `# LOOM_LAUNCH` record for a Codex parent on an operator-exported key,
/// the shape the existing journal tests already use for `codex`.
pub(crate) fn codex_launch() -> String {
    format!(
        "# LOOM_LAUNCH {}",
        serde_json::json!({
            "schema": 1,
            "runtime": "codex",
            "tap": "codex",
            "model": "codex-fixture",
            "credentialSource": "env",
        })
    )
}

/// One OpenCode `step_finish` event. `None` counters are omitted from the
/// event entirely, which is what "unmeasured" looks like on the wire.
pub(crate) fn step(input: u64, output: u64, cache_read: Option<u64>, cost: Option<f64>) -> String {
    let mut tokens = serde_json::json!({"input": input, "output": output});
    if let Some(read) = cache_read {
        tokens["cache"] = serde_json::json!({"read": read});
    }
    let mut event = serde_json::json!({"type": "step_finish", "tokens": tokens});
    if let Some(cost) = cost {
        event["cost"] = serde_json::json!(cost);
    }
    event.to_string()
}

/// One `message_end` event with flat usage, as the Codex parent reports it.
pub(crate) fn message_end(input: u64, output: u64) -> String {
    serde_json::json!({
        "type": "message_end",
        "usage": {"input_tokens": input, "output_tokens": output},
    })
    .to_string()
}

/// The parent sweep's own log on Codex: Curator and Judge ran in-process.
///
/// It also carries the two ways a phase's output reaches the parent's stream
/// in practice, neither of which may count: the `worker run --json` report the
/// orchestrator prints, and a tool result quoting the Builder's phase log from
/// its first line, the `worker run` header included (a JSON string inside
/// another event, never a line of its own).
pub(crate) fn codex_parent_log(sweep_id: &str) -> String {
    codex_parent_log_quoting(sweep_id, true)
}

/// [`codex_parent_log`], with the quoted phase log's header line (`true`) or
/// without it (`false`). The parent's rows must be identical either way: the
/// header must not carry the parent's own `sweep_id=<id>` anchor text.
pub(crate) fn codex_parent_log_quoting(sweep_id: &str, quote_header: bool) -> String {
    let report = serde_json::json!({
        "role": "builder", "issue": ISSUE, "runtime": "opencode",
        "model_profile": "zai-flash", "outcome": "artifact",
    });
    let mut quoted = String::new();
    if quote_header {
        quoted.push_str(&crate::launch_record::phase_log_header(
            Some(sweep_id),
            "builder",
            u64::from(ISSUE),
        ));
        quoted.push('\n');
    }
    quoted.push_str(&format!("{}\n{}", glm_launch(), step(5000, 500, Some(20000), Some(0.05))));
    let quoted_phase_log = serde_json::json!({"type": "tool_result", "output": quoted});
    [
        format!("==== loom-daemon dispatch: sweep_id={sweep_id} issue={ISSUE} ===="),
        "spawn-worker: runtime=codex (from config (runtimes.default))".to_string(),
        "# LOOM_RUNTIME_RESOLVED runtime=codex".to_string(),
        codex_launch(),
        r#"{"type":"agent_start","note":"curator phase"}"#.to_string(),
        message_end(1000, 100),
        report.to_string(),
        quoted_phase_log.to_string(),
        r#"{"type":"agent_start","note":"judge phase"}"#.to_string(),
        message_end(2000, 200),
    ]
    .join("\n")
        + "\n"
}

/// The parent sweep's own log on Claude: no launch record (Claude is not a
/// native harness), and Claude's stream-json usage is read by the transcript
/// path, never by tap accounting.
pub(crate) fn claude_parent_log(sweep_id: &str) -> String {
    format!(
        "==== loom-daemon dispatch: sweep_id={sweep_id} issue={ISSUE} ====\n\
         spawn-claude: selected token FAKE-claude-fixture-token-name\n\
         {{\"type\":\"assistant\",\"message\":{{\"usage\":{{\"input_tokens\":4000,\"output_tokens\":400}}}}}}\n\
         {{\"type\":\"result\",\"usage\":{{\"input_tokens\":4000,\"output_tokens\":400}}}}\n"
    )
}

/// The parent's log on Claude after a #11359 escalation: the orchestrator ran
/// `worker run --role doctor --escalate-claude`, which printed its report and
/// launched nothing (no phase log), then dispatched the Doctor as a Claude
/// Task inside this same session. The Task's stream-json usage is Claude's,
/// read by the transcript path; it has no `# LOOM_LAUNCH` record.
pub(crate) fn claude_parent_log_after_escalation(sweep_id: &str, pr: u64) -> String {
    let report = crate::worker_spawn::escalation_report(Some(pr), "2 GLM Doctor rejections");
    format!(
        "{}{}\n\
         {{\"type\":\"assistant\",\"message\":{{\"model\":\"claude-fixture-opus\",\"usage\":{{\"input_tokens\":9000,\"output_tokens\":900}}}}}}\n\
         {{\"type\":\"result\",\"usage\":{{\"input_tokens\":9000,\"output_tokens\":900}}}}\n",
        claude_parent_log(sweep_id),
        serde_json::to_string(&report).unwrap(),
    )
}

/// A `worker run` phase log: its sweep header, spawn-worker's preamble, the
/// GLM launch record, then `body` (the harness's own event stream).
pub(crate) fn phase_log(role: &str, sweep_id: &str, body: &str) -> String {
    format!(
        "{}\nspawn-worker: runtime=opencode (from config (runtimes.rolePreference.{role}))\n\
         # LOOM_RUNTIME_RESOLVED runtime=opencode\n{}\n{body}\n",
        crate::launch_record::phase_log_header(Some(sweep_id), role, u64::from(ISSUE)),
        glm_launch(),
    )
}

/// The Builder phase of [`mixed_sweep`]: two steps.
pub(crate) fn builder_log(sweep_id: &str) -> String {
    phase_log(
        "builder",
        sweep_id,
        &format!(
            "{}\n{}",
            step(5000, 500, Some(20000), Some(0.05)),
            step(3000, 300, None, Some(0.03))
        ),
    )
}

/// The Doctor phase of [`mixed_sweep`]: one step.
pub(crate) fn doctor_log(sweep_id: &str) -> String {
    phase_log("doctor", sweep_id, &step(700, 70, None, Some(0.007)))
}

/// The hand-computed GLM total from the module table.
pub(crate) fn assert_glm_total(usage: &crate::tap_usage::TapUsage) {
    assert_eq!(usage.input, Some(8700));
    assert_eq!(usage.output, Some(870));
    assert_eq!(usage.cache_read, Some(20000));
    assert_eq!(usage.cache_write, None, "nothing reported cache write");
    assert_eq!(usage.reasoning, None, "nothing reported reasoning");
    assert!((usage.cost_estimate.unwrap() - 0.087).abs() < 1e-9);
    assert_eq!(usage.usage_events, 3);
}
