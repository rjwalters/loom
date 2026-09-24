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
        let kind = event.get("type").and_then(Value::as_str);
        // Kimi's `--output-format stream-json` (2.0.2) is a chat transcript,
        // not a typed event stream: its assistant/tool messages are keyed by
        // `role` and carry no `type` at all, while only its `role:"meta"`
        // lines have one. Counting a `role`-keyed line as an event is what
        // keeps a Kimi launch from degrading to "no opinion" (`events == 0`)
        // and silently keeping its exit-0 success verdict.
        let role = event.get("role").and_then(Value::as_str);
        if kind.is_none() && role.is_none() {
            continue;
        }
        outcome.events += 1;
        if kind.is_some_and(|kind| STEP_FINISH_TYPES.contains(&kind)) {
            outcome.step_finishes += 1;
        }
        if kind.is_some_and(|kind| TOOL_USE_TYPES.contains(&kind))
            && tool_name(&event).is_some_and(is_loom_tool)
        {
            outcome.loom_tool_uses += 1;
        }
        outcome.loom_tool_uses += chat_tool_calls(&event, role);
        if is_denial(&event) {
            outcome.denials += 1;
        }
    }
    outcome
}

/// Whether a harness-reported tool name is one of Loom's guarded tools.
///
/// A harness that namespaces MCP-provided tools reports
/// `mcp__<server>__loom_bash` rather than `loom_bash` (Kimi's
/// `MCP_NAME_PREFIX`), and Loom's own server name carries a per-launch nonce
/// — so the server segment is stripped rather than matched.
fn is_loom_tool(name: &str) -> bool {
    let bare = match name.rsplit_once("__") {
        Some((prefix, bare)) if prefix.starts_with("mcp") => bare,
        _ => name,
    };
    bare.starts_with("loom_")
}

/// Count guarded tool calls carried inside an OpenAI-style assistant
/// message (`{"role":"assistant","tool_calls":[{"function":{"name":…}}]}`),
/// the shape Kimi's stream-json emits. Lenient about where the name sits so
/// a `{"name":…}` variant is recognised too.
fn chat_tool_calls(event: &Value, role: Option<&str>) -> usize {
    if role != Some("assistant") {
        return 0;
    }
    let Some(calls) = event.get("tool_calls").and_then(Value::as_array) else {
        return 0;
    };
    calls
        .iter()
        .filter(|call| {
            call.get("function")
                .and_then(tool_name)
                .or_else(|| tool_name(call))
                .is_some_and(is_loom_tool)
        })
        .count()
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
    fn kimi_stream_json_tool_calls_are_recognized_through_their_mcp_prefix() {
        // The exact shapes `@moonshot-ai/kimi-code` 2.0.2's `PromptJsonWriter`
        // emits under `--output-format stream-json`: `role`-keyed chat
        // messages, with tool calls nested under `tool_calls[].function.name`
        // and namespaced by the MCP server's per-launch name (#8562).
        let stream = "\
            # LOOM_LAUNCH {\"schema\":1,\"runtime\":\"kimi\"}\n\
            {\"role\":\"meta\",\"type\":\"system.version\",\"version\":\"2.0.2\"}\n\
            {\"role\":\"assistant\",\"content\":\"Writing the file.\",\"tool_calls\":[{\"type\":\"function\",\"id\":\"c1\",\"function\":{\"name\":\"mcp__loom-0123456789abcdef__loom_write\",\"arguments\":\"{}\"}}]}\n\
            {\"role\":\"tool\",\"tool_call_id\":\"c1\",\"content\":\"Wrote 3 bytes\"}\n\
            {\"role\":\"assistant\",\"content\":\"Done.\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 1);
        assert_eq!(outcome.events, 4, "meta, two assistant messages and the tool result");
        assert!(!outcome.is_toolless_failure());
    }

    #[test]
    fn a_kimi_run_that_never_reached_a_loom_tool_is_an_observed_toolless_failure() {
        // The deliberately-broken-binding case: the MCP server never loads,
        // so the allowlists leave the model with no tools. It still exits 0
        // and still emits a readable stream — which is exactly why the
        // verdict must come from the stream, not the exit code.
        let stream = "\
            {\"role\":\"meta\",\"type\":\"system.version\",\"version\":\"2.0.2\"}\n\
            {\"role\":\"assistant\",\"content\":\"I have no tools available.\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 0);
        assert_eq!(outcome.events, 2);
        assert!(outcome.observed_a_toolless_run());
    }

    #[test]
    fn a_kimi_run_using_only_its_own_builtin_tools_is_still_a_toolless_failure() {
        // If the allowlists ever fell open, Kimi would report its OWN tools
        // ("Bash"/"Write"), or another MCP server's. Neither counts.
        let stream = "\
            {\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"Bash\"}}]}\n\
            {\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"mcp__other__loom_write\"}}]}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 1, "only the loom-prefixed server's tool counts");
        let only_builtin = classify_native_stream(
            "{\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"Write\"}}]}\n",
        );
        assert!(only_builtin.observed_a_toolless_run());
    }

    #[test]
    fn a_denied_kimi_tool_result_still_counts_as_a_working_binding() {
        let stream = "\
            {\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"mcp__loom-ab__loom_bash\"}}]}\n\
            {\"role\":\"tool\",\"tool_call_id\":\"c1\",\"content\":\"denied by Loom policy: protected branch\"}\n\
        ";
        let outcome = classify_native_stream(stream);
        assert_eq!(outcome.loom_tool_uses, 1);
        assert_eq!(outcome.denials, 1);
        assert!(!outcome.is_toolless_failure());
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
