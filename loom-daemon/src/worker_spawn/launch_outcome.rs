//! Post-hoc classification of a completed native-harness launch's captured
//! stdout/log for whether it ever actually used a guarded `loom_*` tool
//! (#8448). A native launch `exec`s (see `worker_spawn::run`), so nothing in
//! THIS process supervises it after that point — this is deliberately a pure
//! function of already-captured text, called from whichever process DID
//! supervise the launch and holds its log. Today that is the role runner
//! (`role_runner::toolless_launch`, which turns this into a
//! `RoleTickOutcome::Failure` instead of an exit-0 `Success`).
//!
//! Why this is needed: a guarded launch on a major whose binding fails to
//! load can silently degrade to the deny-by-default agent with NO tools at
//! all (#8448's live receipt on OpenCode 2.0.10) — the model does nothing,
//! and the harness still exits 0. Exit code alone cannot distinguish that
//! from a real, tool-using completion.
//!
//! The exact JSON shape of each harness's `--format json` / native event
//! stream is not something a fixture can pin down for a real CLI (see
//! `.loom/docs/guardrail-parity-native.md`); this classifier is deliberately
//! lenient about event-type spelling (`tool_use`/`tool-use`,
//! `step_finish`/`step-finish`) and about which field carries the tool name
//! (`tool`/`name`/`tool_name`/`toolName`), and ignores anything that is not a
//! JSON object on its own line — launch-record headers (`# LOOM_LAUNCH …`),
//! `# LOOM_CLI_START`, and any other log prose a `--log` file interleaves in.
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaunchOutcome {
    /// How many events looked like a `loom_*` tool actually being invoked
    /// (whether or not Loom's policy then allowed the effect through).
    pub loom_tool_uses: usize,
    /// How many `step_finish`-shaped events were seen. Corroborating
    /// evidence only — classification never keys off this alone; see the
    /// module doc for why (a run that finishes without ever touching a
    /// `loom_*` tool is exactly the failure this exists to catch).
    pub step_finishes: usize,
    /// Best-effort count of guard-denial-shaped tool results (the tool ran,
    /// Loom's policy said no). Never authoritative: useful only as an
    /// operator-facing hint alongside a canary run's independent file-hash
    /// checks, which are the real evidence.
    pub denials: usize,
    /// How many lines parsed as a native event at all (a JSON object with a
    /// string `type`). This is the classifier's own "could I read this
    /// stream?" signal, and is what keeps [`Self::is_toolless_failure`] from
    /// being usable as a verdict on text this module did not understand: a
    /// caller acting on a toolless verdict MUST require `events > 0` first
    /// (see [`Self::observed_a_toolless_run`]), so a harness whose stream
    /// format this module cannot parse degrades to "no opinion" rather than
    /// to "every launch failed".
    pub events: usize,
}

impl LaunchOutcome {
    /// A guarded role launch that never actually invoked a `loom_*` tool is a
    /// failed launch regardless of exit code (#8448) — the binding may have
    /// failed to load, leaving the model with nothing to do; or, if a broken
    /// binding ever fell OPEN instead of closed, the model used the
    /// harness's own unguarded tools instead, which this counts the same
    /// way: not a `loom_*` use.
    pub fn is_toolless_failure(&self) -> bool {
        self.loom_tool_uses == 0
    }

    /// The form of [`Self::is_toolless_failure`] a caller may actually act on:
    /// a toolless run **that this module could read**. An unparsed stream
    /// (`events == 0`) is a gap in Loom's own observation, never evidence
    /// about the launch — see [`Self::events`].
    pub fn observed_a_toolless_run(&self) -> bool {
        self.events > 0 && self.is_toolless_failure()
    }
}

const TOOL_USE_TYPES: &[&str] = &["tool_use", "tool-use", "tool_call", "tool-call"];
const STEP_FINISH_TYPES: &[&str] = &["step_finish", "step-finish"];
const TOOL_NAME_FIELDS: &[&str] = &["tool", "name", "tool_name", "toolName"];
const DENIAL_PHRASES: &[&str] = &[
    "denied by loom policy",
    "native policy check failed",
    "refusing tool",
    "denied by policy",
];

/// Classify a completed launch's captured stdout/log text. Order-independent
/// and line-oriented: any line that parses as a JSON object is inspected;
/// every other line is ignored rather than rejected, since a captured
/// `--log` interleaves Loom's own marker lines with the harness's own
/// stream (see `worker_spawn::run`'s `# LOOM_LAUNCH` header).
pub fn classify_native_stream(text: &str) -> LaunchOutcome {
    let mut outcome = LaunchOutcome::default();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        outcome.events += 1;
        if STEP_FINISH_TYPES.contains(&kind) {
            outcome.step_finishes += 1;
        }
        if TOOL_USE_TYPES.contains(&kind)
            && tool_name(&event).is_some_and(|n| n.starts_with("loom_"))
        {
            outcome.loom_tool_uses += 1;
        }
        if is_denial(&event) {
            outcome.denials += 1;
        }
    }
    outcome
}

fn tool_name(event: &Value) -> Option<&str> {
    TOOL_NAME_FIELDS
        .iter()
        .find_map(|field| event.get(*field).and_then(Value::as_str))
}

fn is_denial(event: &Value) -> bool {
    let text = event_text(event).to_lowercase();
    DENIAL_PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// Denial text can live under several shapes depending on how a harness
/// reports a failed tool call; concatenate everything textual rather than
/// pinning one field path an unverified CLI might not use.
fn event_text(event: &Value) -> String {
    let mut out = String::new();
    for field in ["text", "output", "error", "content"] {
        match event.get(field) {
            Some(Value::String(s)) => {
                out.push_str(s);
                out.push(' ');
            }
            Some(other) => {
                out.push_str(&other.to_string());
                out.push(' ');
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toolless_run_is_a_failure_even_with_step_start_and_text_events() {
        // The exact shape #8448's live receipt observed on OpenCode 2.0.10:
        // one step_start, one text event, no tool_use, no step_finish.
        let stream = "\
            # LOOM_LAUNCH {\"schema\":1,\"runtime\":\"opencode\"}\n\
            spawn-worker: runtime=opencode (from env)\n\
            {\"type\":\"step_start\"}\n\
            {\"type\":\"text\",\"text\":\"I do not have tools named loom_write\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 0);
        assert_eq!(outcome.step_finishes, 0);
        assert_eq!(outcome.events, 2, "the two JSON events, not the prose lines");
        assert!(outcome.is_toolless_failure());
        assert!(outcome.observed_a_toolless_run());
    }

    #[test]
    fn a_run_that_actually_used_a_loom_tool_is_not_a_failure() {
        let stream = "\
            {\"type\":\"step_start\"}\n\
            {\"type\":\"tool_use\",\"tool\":\"loom_write\",\"input\":{\"path\":\"a.txt\"}}\n\
            {\"type\":\"tool_result\",\"tool\":\"loom_write\",\"output\":\"Wrote 3 bytes\"}\n\
            {\"type\":\"step_finish\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 1);
        assert_eq!(outcome.step_finishes, 1);
        assert!(!outcome.is_toolless_failure());
    }

    #[test]
    fn a_denied_loom_tool_call_still_counts_as_a_use_the_binding_loaded() {
        // The binding loaded (the model could call loom_bash at all) — Loom's
        // policy then said no. That is a working binding, a denied ACTION,
        // never the toolless-launch failure this module exists to catch.
        let stream = "\
            {\"type\":\"tool_use\",\"tool\":\"loom_bash\",\"input\":{\"command\":\"git push --force origin main\"}}\n\
            {\"type\":\"tool_result\",\"tool\":\"loom_bash\",\"error\":\"denied by Loom policy: protected branch\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 1);
        assert_eq!(outcome.denials, 1);
        assert!(!outcome.is_toolless_failure());
    }

    #[test]
    fn a_fallen_open_run_using_only_unguarded_tools_is_still_a_toolless_failure() {
        // If a broken binding ever fell OPEN instead of closed, the model
        // would use the harness's OWN tools ("write", not "loom_write").
        // Only loom_* counts — anything else must not silently pass.
        let stream = "\
            {\"type\":\"tool_use\",\"name\":\"write\",\"input\":{\"filePath\":\"a.txt\"}}\n\
            {\"type\":\"step_finish\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 0);
        assert!(outcome.is_toolless_failure());
    }

    #[test]
    fn hyphenated_event_spellings_and_alternate_name_fields_are_recognized() {
        let stream = "\
            {\"type\":\"tool-use\",\"tool_name\":\"loom_edit\"}\n\
            {\"type\":\"step-finish\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 1);
        assert_eq!(outcome.step_finishes, 1);
    }

    #[test]
    fn non_json_and_malformed_lines_are_ignored_not_fatal() {
        let stream = "plain text\n{not json\n{\"type\":123}\n{}\n\n";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome, LaunchOutcome::default());
    }

    #[test]
    fn an_unreadable_stream_is_no_opinion_not_a_verdict() {
        // Nothing here parsed as a native event, so the classifier saw no
        // stream at all. `is_toolless_failure` is trivially true (zero
        // `loom_*` uses), which is exactly why callers must gate on
        // `observed_a_toolless_run` instead: an unrecognized stream format
        // must degrade to "no opinion", never to "every launch failed".
        for stream in ["", "plain text\n", "{not json\n{\"type\":123}\n{}\n"] {
            let outcome = classify_native_stream(stream);
            assert_eq!(outcome.events, 0, "{stream:?}");
            assert!(outcome.is_toolless_failure(), "{stream:?}");
            assert!(!outcome.observed_a_toolless_run(), "{stream:?}");
        }
    }
}
