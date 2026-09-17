//! Extract-and-execute support for `../sweep_md_doc_lint.rs` (#7993, split
//! from #7979).
//!
//! These functions hold the bodies of the doc-lint checks that EXECUTE
//! extracted content (a fenced shell snippet run against stub scripts, or a
//! fenced JSON wire frame deserialized through the real
//! `loom_daemon::types::{Request, Response, Event}` types) rather than
//! string-matching it. They live in a sibling module — not inline in
//! `sweep_md_doc_lint.rs` — purely to keep that file under the file-size
//! ratchet threshold (`.loom/docs/file-size-policy.md`); each `#[test]` in
//! that file is a thin one-line dispatch into the matching function here.
//!
//! See `sweep_md_doc_lint.rs`'s own module doc for the incident (#7876,
//! #7950, #7948) this migration exists to prevent: a behavior-preserving
//! reword of fenced/tabled text must not fail these checks — only an actual
//! behavior change may.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use loom_daemon::rate_limit_breaker::indicates_rate_limit;
use loom_daemon::types::{Event, Request, Response, SweepKind, SweepOutcome};
use std::fs;
use std::path::Path;
use std::process::Command;

/// Extracts the body of the first fenced code block (```` ```lang\n...\n``` ````)
/// that starts at or after byte offset `from` in `content`, and returns
/// (block body, byte offset immediately after the closing fence).
///
/// Used to locate a shell/JSON example anchored by nearby prose (a heading or
/// a sentence naming it) without pinning the fenced content itself as a
/// string — the fence is EXECUTED or DESERIALIZED by the caller instead.
/// Panics with a message naming `label` if no fence is found, or if it is
/// never closed — exactly the "someone deleted/broke the example" case this
/// lint exists to catch.
pub fn first_fenced_block_after<'a>(
    content: &'a str,
    from: usize,
    label: &str,
) -> (&'a str, usize) {
    let rest = &content[from..];
    let open = rest
        .find("```")
        .unwrap_or_else(|| panic!("expected a fenced code block after `{label}`, found none"));
    let after_open = &rest[open + 3..];
    let nl = after_open.find('\n').unwrap_or_else(|| {
        panic!("malformed fence after `{label}`: no newline following the opening ```")
    });
    let body = &after_open[nl + 1..];
    let close = body.find("```").unwrap_or_else(|| {
        panic!("unterminated fence after `{label}` — the code block never closes")
    });
    (&body[..close], from + open + 3 + nl + 1 + close + 3)
}

/// Writes `body` as an executable bash script at `path`, with a `#!/usr/bin/env
/// bash` shebang prepended. Shared by the Step 1a/1b extract-and-execute tests.
pub fn write_executable_stub(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\n{body}")).expect("write stub script");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

/// Writes an executable stub for `$GH_READ` that logs its argv to `log_path`
/// and behaves like `gh api --paginate <endpoint> --jq <filter>`: it applies
/// the REAL `--jq` filter (via the system `jq` binary) to `fixture`, so the
/// extracted fence's own filter logic is what runs, not a re-implementation
/// of it in Rust.
fn build_gh_read_stub(
    sandbox: &tempfile::TempDir,
    log_path: &Path,
    fixture: &serde_json::Value,
) -> std::path::PathBuf {
    let suffix = log_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("gh-read")
        .to_string();
    let fixture_path = sandbox.path().join(format!("fixture-{suffix}.json"));
    fs::write(&fixture_path, fixture.to_string()).expect("write fixture json");
    let stub_path = sandbox.path().join(format!("gh-read-stub-{suffix}.sh"));
    write_executable_stub(
        &stub_path,
        &format!(
            "echo \"$@\" >> {log_path:?}\n\
             FILTER=\"\"\n\
             while [[ $# -gt 0 ]]; do\n\
             \x20\x20case \"$1\" in\n\
             \x20\x20\x20\x20--jq) FILTER=\"$2\"; shift 2;;\n\
             \x20\x20\x20\x20*) shift;;\n\
             \x20\x20esac\n\
             done\n\
             jq \"$FILTER\" {fixture_path:?}\n"
        ),
    );
    stub_path
}

/// AC #3 (#7993 migration — table/structure assertion, executable surface
/// exists): assert all six initial topics are present in the markdown AND
/// that the REAL `Event::topic()` implementation actually produces each
/// documented pattern. Constructing each variant and calling the real method
/// (rather than only string-matching sweep.md) catches drift in either
/// direction: a Rust rename that forgets to update sweep.md, or a sweep.md
/// edit that no longer matches what the code actually emits.
pub fn check_topic_taxonomy(content: &str, required_topics: &[&str]) {
    // Doc presence (not a code-fence literal — the topic table lives in a
    // plain markdown table row, not inside a fence): still worth keeping,
    // it is what actually appears in sweep.md — the real Event::topic()
    // check below is what makes it EXECUTABLE rather than merely textual.
    for topic in required_topics {
        assert!(
            content.contains(topic),
            "sweep.md is missing topic `{topic}` from the Phase B taxonomy; \
             update sweep.md or this test if the change is intentional"
        );
    }

    // EXECUTE: build one instance of each of the six documented variants
    // (issue 42 for the four per-issue topics) and assert the real
    // `Event::topic()` matches the pattern from `required_topics` with `{N}`
    // substituted for the concrete issue number used here.
    let per_issue: &[(Event, &str)] = &[
        (
            Event::SweepPhase {
                issue: 42,
                phase: "builder".to_string(),
                pr_number: None,
                repo: None,
            },
            "sweep.issue.{N}.phase",
        ),
        (
            Event::SweepBlocker {
                issue: 42,
                reason: "missing credentials".to_string(),
                label_added: "loom:operator-only".to_string(),
                repo: None,
            },
            "sweep.issue.{N}.blocker",
        ),
        (
            Event::SweepExited {
                issue: 42,
                exit_code: Some(0),
                duration_sec: 1,
                no_progress: false,
                death_class: None,
                repo: None,
            },
            "sweep.issue.{N}.exited",
        ),
        (
            Event::SweepCrashed {
                issue: 42,
                checkpoint_phase: None,
                classification: None,
                death_class: None,
                repo: None,
            },
            "sweep.issue.{N}.crashed",
        ),
    ];
    for (event, pattern) in per_issue {
        let expected = pattern.replace("{N}", "42");
        assert_eq!(
            event.topic(),
            expected,
            "Event::topic() for the constructed variant no longer matches the \
             documented pattern `{pattern}` (#7993 executable topic-taxonomy check)"
        );
    }

    let global: &[(Event, &str)] = &[
        (
            Event::SweepGlobalDispatch {
                sweep_id: "sweep-issue-42-1717599600".to_string(),
                kind: SweepKind::Issue(42),
                runtime: None,
                runtime_source: None,
                repo: None,
            },
            "sweep.global.dispatch",
        ),
        (
            Event::SweepGlobalCompleted {
                sweep_id: "sweep-issue-42-1717599600".to_string(),
                outcome: SweepOutcome::Exited,
            },
            "sweep.global.completed",
        ),
    ];
    for (event, expected) in global {
        assert_eq!(
            event.topic(),
            *expected,
            "Event::topic() for the constructed variant no longer matches the \
             documented global topic `{expected}` (#7993 executable topic-taxonomy check)"
        );
    }
}

/// AC #3 (#7993 migration — code-fence literal, converted to extract +
/// deserialize): the "Sample wire frame" request/response pair under "How to
/// publish — IPC contract" is extracted from its fences and deserialized
/// through the REAL `loom_daemon::types::Request`/`Response` wire types,
/// rather than string-matching the JSON text. A wire-format change that
/// breaks deserialization now fails loudly on the exact doc example instead
/// of silently drifting.
pub fn check_publish_event_ipc_contract(content: &str) {
    // PROSE (structural, not fenced — a plain sentence naming the variant):
    // still worth a presence check since it is genuinely prose, not a fence
    // literal.
    let anchor = content
        .find("Sample wire frame")
        .expect("sweep.md must document a \"Sample wire frame\" example for the IPC contract");
    assert!(
        content[..anchor].contains("Request::PublishEvent"),
        "sweep.md should reference `Request::PublishEvent` before its sample wire \
         frame — the IPC contract is required by #3453 AC #3"
    );

    // EXECUTE: deserialize the request fence through the real wire type.
    let (request_json, after_request) =
        first_fenced_block_after(content, anchor, "Sample wire frame");
    let request: Request = serde_json::from_str(request_json.trim()).unwrap_or_else(|e| {
        panic!("sweep.md's sample PublishEvent request frame no longer deserializes as `Request`: {e}\nJSON: {request_json}")
    });
    match request {
        Request::PublishEvent { topic, payload } => {
            assert_eq!(topic, "sweep.issue.123.phase");
            assert_eq!(payload["phase"], "builder");
            assert_eq!(payload["pr_number"], 501);
        }
        other => panic!("expected Request::PublishEvent, got {other:?}"),
    }

    // EXECUTE: deserialize the paired response fence.
    let responds_at = content[after_request..]
        .find("The daemon responds with:")
        .expect("sweep.md must show the daemon's response to the sample PublishEvent request")
        + after_request;
    let (response_json, _) =
        first_fenced_block_after(content, responds_at, "The daemon responds with:");
    let response: Response = serde_json::from_str(response_json.trim()).unwrap_or_else(|e| {
        panic!("sweep.md's sample EventPublished response frame no longer deserializes as `Response`: {e}\nJSON: {response_json}")
    });
    match response {
        Response::EventPublished { topic, receivers } => {
            assert_eq!(topic, "sweep.issue.123.phase");
            assert_eq!(receivers, 2);
        }
        other => panic!("expected Response::EventPublished, got {other:?}"),
    }

    // EXECUTE: the Subscription section's sample frame, same treatment.
    let sub_anchor = content
        .find("Long-running monitors subscribe with a single")
        .expect("sweep.md must document the SubscribeEvents subscription contract");
    let (sub_json, _) = first_fenced_block_after(
        content,
        sub_anchor,
        "Long-running monitors subscribe with a single",
    );
    let sub_request: Request = serde_json::from_str(sub_json.trim()).unwrap_or_else(|e| {
        panic!("sweep.md's sample SubscribeEvents request frame no longer deserializes as `Request`: {e}\nJSON: {sub_json}")
    });
    match sub_request {
        Request::SubscribeEvents { topics } => {
            assert_eq!(topics, vec!["sweep.issue.123", "sweep.global.completed"]);
        }
        other => panic!("expected Request::SubscribeEvents, got {other:?}"),
    }
}

/// AC #3 (#7993 migration — code-fence literal, converted to extract +
/// deserialize): "Sample payloads for the six initial topics" and the
/// daemon-side events fence right after it are extracted and deserialized
/// through the real `Request`/`Response`/`Event` wire types. For the
/// daemon-emitted events this goes one step further than deserialization —
/// it calls the REAL `Event::topic()` on the parsed value and asserts it
/// matches the topic the doc's own prose claims for that sample, so a
/// behavior change (not just a wording change) is what fails this test.
pub fn check_sample_wire_payloads(content: &str) {
    let anchor = content
        .find("authoritative reference for the payload schema")
        .expect("sweep.md must introduce the six sample payloads as authoritative");
    let (samples_block, after_samples) =
        first_fenced_block_after(content, anchor, "authoritative reference for the payload schema");

    let mut phase_count = 0;
    let mut blocker_count = 0;
    for line in samples_block.lines().filter(|l| !l.trim().is_empty()) {
        let request: Request = serde_json::from_str(line.trim()).unwrap_or_else(|e| {
            panic!("sweep.md's sample payload line no longer deserializes as `Request`: {e}\nline: {line}")
        });
        match request {
            Request::PublishEvent { topic, .. } if topic.ends_with(".phase") => phase_count += 1,
            Request::PublishEvent { topic, .. } if topic.ends_with(".blocker") => {
                blocker_count += 1
            }
            other => panic!("unexpected sample payload variant: {other:?}"),
        }
    }
    assert!(
        phase_count >= 1 && blocker_count >= 1,
        "sweep.md's six sample payloads must include at least one `.phase` and \
         one `.blocker` PublishEvent sample (#3453 AC #3); found {phase_count} \
         phase, {blocker_count} blocker"
    );

    // EXECUTE: the daemon-side events fence — deserialize as `Response`,
    // unwrap to the real `Event`, and call the real `.topic()` method,
    // asserting it lands on the documented topic FAMILY for that sample
    // (SweepExited -> .exited, SweepCrashed -> .crashed, the two globals).
    let daemon_anchor = content[after_samples..]
        .find("these are **emitted by the daemon**, not by the sweep child")
        .expect("sweep.md must show the daemon-emitted event samples")
        + after_samples;
    let (daemon_block, _) = first_fenced_block_after(
        content,
        daemon_anchor,
        "these are **emitted by the daemon**, not by the sweep child",
    );

    let mut seen_suffixes = std::collections::HashSet::new();
    for line in daemon_block.lines().filter(|l| !l.trim().is_empty()) {
        let response: Response = serde_json::from_str(line.trim()).unwrap_or_else(|e| {
            panic!("sweep.md's daemon-side sample line no longer deserializes as `Response`: {e}\nline: {line}")
        });
        let events = match response {
            Response::EventStream { events } => events,
            other => panic!("expected Response::EventStream, got {other:?}"),
        };
        for event in events {
            let topic = event.topic();
            assert!(
                topic.starts_with("sweep.issue.") || topic.starts_with("sweep.global."),
                "unexpected topic `{topic}` for daemon-side sample event {event:?}"
            );
            let suffix = topic.rsplit('.').next().unwrap_or_default().to_string();
            seen_suffixes.insert(suffix);
        }
    }
    for expected_suffix in ["exited", "crashed", "dispatch", "completed"] {
        assert!(
            seen_suffixes.contains(expected_suffix),
            "sweep.md's daemon-side event samples are missing a `.{expected_suffix}` \
             example, or the real Event::topic() no longer resolves one to it \
             (#3453 AC #3 requires sample payloads for each of the six topics); \
             saw: {seen_suffixes:?}"
        );
    }
}

/// #4670 (#7993 migration — table/structure assertion, executable surface
/// exists): the five signatures the doc's table lists are cross-checked
/// against the REAL `loom_daemon::rate_limit_breaker::indicates_rate_limit`
/// function, not just matched as text. If the doc lists a signature the real
/// detector no longer recognizes (or vice versa), this fails on the actual
/// classification behavior instead of only on wording.
pub fn check_rate_limit_signature_table(content: &str) {
    let signatures: &[&str] = &[
        "api rate limit exceeded",
        "api rate limit already exceeded",
        "secondary rate limit",
        "abuse detection mechanism",
        "was submitted too quickly",
    ];
    for signature in signatures {
        assert!(
            content.contains(signature),
            "sweep.md's GraphQL-exhaustion detection is missing the rate-limit \
             signature `{signature}` (#4670) — the table must mirror \
             check-duplicate.sh's is_rate_limit_error() / \
             rate_limit_breaker.rs's RATE_LIMIT_SIGNATURES, not a new one"
        );
        // EXECUTE: the real detector must actually recognize this exact
        // documented signature, embedded in a realistic gh error string.
        let simulated = format!("HTTP 403: {signature} for user ID 12345");
        assert!(
            indicates_rate_limit(&simulated),
            "sweep.md documents `{signature}` as a rate-limit signature, but \
             the REAL `indicates_rate_limit()` (loom-daemon/src/rate_limit_breaker.rs) \
             does not recognize it — the doc and the code have drifted (#4670)"
        );
    }

    // CONTRACT: the provenance pointers keep future editors on the shared
    // table instead of re-deriving one. Keep EXACT (file/symbol names) — these
    // are plain prose citations, not fenced code.
    for needle in ["is_rate_limit_error()", "RATE_LIMIT_SIGNATURES"] {
        assert!(
            content.contains(needle),
            "sweep.md must cite `{needle}` as the source of the rate-limit \
             signature table (#4670)"
        );
    }
}

/// This lint is the mechanical guard against a future edit "simplifying" the
/// gate away.
///
/// #7993 migration (this is the exact fence #7876/#7950/#7948 broke on): this
/// used to `content.contains("sweep-lease-renew.sh start \"$N\"")` — a literal
/// pinned inside the Step 1a code fence. #7876 legitimately renamed the
/// downstream Step 1b variables from positional (`$1`/`$2`) to named
/// (`$LEASE_HOST`/`$LEASE_SWEEP`) for a real zsh word-splitting fix, unrelated
/// to Step 1a's fence, and a *different* red-main incident along the same
/// fault line is exactly what this migration exists to make impossible: this
/// EXTRACTS the fence and RUNS it against a stub `sweep-lease-renew.sh`,
/// asserting the marker-gated behavior itself (skip when the marker names
/// `$N`; fall back to `start` otherwise) rather than one specific spelling of
/// the invocation.
pub fn check_step_1a_lease_renewal_fallback(content: &str) {
    // `.rfind()`, not `.find()`: "Step 1a — daemon self-claim check" is
    // legitimately forward-referenced (twice) from the flag documentation in
    // `sweep-arguments.md`, earlier in file order than the actual heading in
    // `sweep-wave-lifecycle.md`'s "1. Per-issue pre-flight" section — the
    // fence we want to extract sits under the real heading, the last
    // occurrence (see `check_step_1b_lease_publish`'s identical comment).
    let anchor = content
        .rfind("Step 1a — daemon self-claim check")
        .expect("sweep.md must retain the `Step 1a — daemon self-claim check` anchor (#4111)");
    let (fence, _) = first_fenced_block_after(content, anchor, "Step 1a — daemon self-claim check");

    // The fence must still reference the shared capability-marker env var —
    // this is the identifier `LEASE_RENEW_STARTED_ENV` in
    // `loom-daemon/src/sweep_registry/dispatch.rs` mirrors; a rename on one
    // side without the other breaks the gate (checked structurally, since the
    // *value* of that check is exercised below).
    assert!(
        fence.contains("LOOM_SWEEP_LEASE_RENEW_DISPATCHED"),
        "Step 1a's fenced fallback snippet must gate on `LOOM_SWEEP_LEASE_RENEW_DISPATCHED` \
         (#7672) — without the gate, a new prompt running against a pre-#7672 daemon \
         binary starts no renewal loop at all and its lease ages into a peer's \
         reclamation gate (#6286). Fence body:\n{fence}"
    );

    let sandbox = tempfile::tempdir().expect("create sandbox dir");
    let scripts_dir = sandbox.path().join(".loom/scripts");
    fs::create_dir_all(&scripts_dir).expect("create .loom/scripts");
    let log_path = sandbox.path().join("renew-calls.log");
    write_executable_stub(
        &scripts_dir.join("sweep-lease-renew.sh"),
        &format!("echo \"$@\" >> {log_path:?}\n"),
    );

    let run = |n: &str, dispatched: Option<&str>| -> String {
        let _ = fs::remove_file(&log_path);
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(fence)
            .current_dir(sandbox.path())
            .env("N", n)
            .env_remove("LOOM_SWEEP_LEASE_RENEW_DISPATCHED");
        if let Some(d) = dispatched {
            cmd.env("LOOM_SWEEP_LEASE_RENEW_DISPATCHED", d);
        }
        let status = cmd.status().expect("run extracted Step 1a fence");
        assert!(status.success(), "Step 1a's extracted fence exited non-zero");
        fs::read_to_string(&log_path).unwrap_or_default()
    };

    // Marker names THIS issue: the daemon already started the loop -> skip.
    let log = run("42", Some("42"));
    assert!(
        log.is_empty(),
        "Step 1a's fence must NOT invoke `sweep-lease-renew.sh start` when \
         LOOM_SWEEP_LEASE_RENEW_DISPATCHED already names this issue (#7672) — \
         invoking it anyway forks a duplicate PATCH loop; log: {log:?}"
    );

    // Marker unset (pre-#7672 daemon, or an in-session run): fall back to start.
    let log = run("42", None);
    assert!(
        log.trim() == "start 42",
        "Step 1a's fence must invoke `sweep-lease-renew.sh start 42` when the \
         capability marker is unset (#6180 fallback) — got: {log:?}"
    );

    // Marker names a DIFFERENT issue (this is issue 42 in a wave the daemon
    // dispatched issue 99 for): still falls back to start for 42.
    let log = run("42", Some("99"));
    assert!(
        log.trim() == "start 42",
        "Step 1a's fence must still fall back to `start 42` when the marker \
         names a different issue than the one being pre-flighted — got: {log:?}"
    );
}

/// #4670 (#7993 migration — code-fence literal, converted to extract +
/// execute): the Mode B / Mode C REST-fallback `gh api --paginate ...` one-
/// liners are extracted from their fences and actually RUN against a stub
/// `$GH_READ`, with the real `--jq` filter they embed applied for real (via
/// `jq`) to fixture JSON. This exercises the actual PR-exclusion / label
/// filtering behavior, not just the presence of the flag/endpoint text.
pub fn check_rest_fallback_endpoints_and_pagination(content: &str) {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("SKIP: jq not available in this environment");
        return;
    }

    // ---- Mode B: `repos/{owner}/{repo}/issues`, PRs excluded ----
    let mode_b_anchor = content
        .find("GraphQL-exhaustion fallback (REST issue discovery")
        .expect("sweep.md must document Mode B's GraphQL-exhaustion fallback");
    let (mode_b_fence, _) = first_fenced_block_after(
        content,
        mode_b_anchor,
        "GraphQL-exhaustion fallback (REST issue discovery",
    );
    assert!(
        mode_b_fence.contains("repos/{owner}/{repo}/issues")
            && mode_b_fence.contains("--paginate")
            && mode_b_fence.contains("per_page=100")
            && mode_b_fence.contains("state=open"),
        "Mode B's REST fallback fence is missing an expected flag/endpoint \
         (#4670). Fence body:\n{mode_b_fence}"
    );

    let sandbox = tempfile::tempdir().expect("create sandbox dir");
    let gh_read_log = sandbox.path().join("gh_read.log");
    let fixture = serde_json::json!([
        {"number": 10, "title": "a real issue", "pull_request": null,
         "labels": [{"name": "loom:issue"}], "updated_at": "2026-09-01T00:00:00Z"},
        {"number": 11, "title": "a PR that /issues also returns", "pull_request": {"url": "x"},
         "labels": [{"name": "loom:review-requested"}], "updated_at": "2026-09-01T00:00:00Z"}
    ]);
    let gh_read_stub = build_gh_read_stub(&sandbox, &gh_read_log, &fixture);

    let output = Command::new("bash")
        .arg("-c")
        .arg(mode_b_fence)
        .current_dir(sandbox.path())
        .env("GH_READ", &gh_read_stub)
        .output()
        .expect("run Mode B REST fallback fence");
    assert!(
        output.status.success(),
        "Mode B fence exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let call_log = fs::read_to_string(&gh_read_log).unwrap_or_default();
    assert!(
        call_log.contains("repos/{owner}/{repo}/issues") && call_log.contains("--paginate"),
        "the stub $GH_READ was not invoked with the expected Mode B endpoint/flags; \
         call log: {call_log:?}"
    );
    let filtered: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("Mode B fence's --jq output is not valid JSON: {e}"));
    let numbers: Vec<i64> = filtered
        .as_array()
        .expect("jq output must be an array")
        .iter()
        .map(|v| v["number"].as_i64().unwrap())
        .collect();
    assert_eq!(
        numbers,
        vec![10],
        "Mode B's `select(.pull_request == null)` filter must drop the PR-shaped \
         item (#11) and keep only the real issue (#10) — #4670's whole point is \
         that `/issues` also returns PRs"
    );

    // ---- Mode C: `repos/{owner}/{repo}/pulls`, label-filtered client-side ----
    let mode_c_anchor = content
        .find("GraphQL-exhaustion fallback (REST PR discovery")
        .expect("sweep.md must document Mode C's GraphQL-exhaustion fallback");
    let (mode_c_fence, _) = first_fenced_block_after(
        content,
        mode_c_anchor,
        "GraphQL-exhaustion fallback (REST PR discovery",
    );
    assert!(
        mode_c_fence.contains("repos/{owner}/{repo}/pulls")
            && mode_c_fence.contains("--paginate")
            && mode_c_fence.contains("per_page=100")
            && mode_c_fence.contains("state=open"),
        "Mode C's REST fallback fence is missing an expected flag/endpoint \
         (#4670). Fence body:\n{mode_c_fence}"
    );

    let fixture_c = serde_json::json!([
        {"number": 20, "title": "awaiting judge", "labels": [{"name": "loom:review-requested"}]},
        {"number": 21, "title": "not review-requested", "labels": [{"name": "loom:pr"}]}
    ]);
    let gh_read_log_c = sandbox.path().join("gh_read_c.log");
    let gh_read_stub_c = build_gh_read_stub(&sandbox, &gh_read_log_c, &fixture_c);

    let output = Command::new("bash")
        .arg("-c")
        .arg(mode_c_fence)
        .current_dir(sandbox.path())
        .env("GH_READ", &gh_read_stub_c)
        .output()
        .expect("run Mode C REST fallback fence");
    assert!(
        output.status.success(),
        "Mode C fence exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let filtered: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("Mode C fence's --jq output is not valid JSON: {e}"));
    let numbers: Vec<i64> = filtered
        .as_array()
        .expect("jq output must be an array")
        .iter()
        .map(|v| v["number"].as_i64().unwrap())
        .collect();
    assert_eq!(
        numbers,
        vec![20],
        "Mode C's client-side `loom:review-requested` label filter must keep \
         only PR #20 and drop #21 (#4670)"
    );

    // The unknown-label guard's own REST rung (`repos/{owner}/{repo}/labels`)
    // is plain prose naming the endpoint, not a fenced executable snippet —
    // still worth a presence check.
    assert!(
        content.contains("repos/{owner}/{repo}/labels"),
        "sweep.md must document the label-guard's REST rung `repos/{{owner}}/{{repo}}/labels` (#4670)"
    );
}

/// #6320: "Step 1b" must exist, must invoke `sweep-lease-publish.sh`, must
/// come AFTER Step 1a (which covers the daemon-claimed case and must not be
/// duplicated by it), and must document the peer-lease skip.
///
/// #7993 migration (this is the exact incident site of #7876/#7950/#7948):
/// the byte-offset ordering checks and the `Exit \`4\`` prose mention stay as
/// structural/PROSE checks (neither lives in a fence), but the fenced
/// `sweep-lease-publish.sh` / `sweep-lease-renew.sh start` invocation itself
/// is now extracted and RUN against stub scripts, asserting the actual
/// flag/value threading behavior — the thing #7876 changed the SPELLING of
/// without changing — instead of one pinned literal spelling of it.
pub fn check_step_1b_lease_publish(content: &str) {
    // Anchor on the section headings themselves, not on the first mention of
    // "Step 1a"/"Step 1b" anywhere in the file — both are legitimately
    // forward-referenced from the flag documentation far above pre-flight.
    let step_1a_pos = content
        .find("Step 1a — daemon self-claim check")
        .expect("sweep.md must retain the `Step 1a — daemon self-claim check` anchor (#4111)");
    let step_1b_pos = content
        .find("Step 1b — publish this sweep's OWN lease record")
        .unwrap_or_else(|| {
            panic!(
                "sweep.md is missing the `Step 1b — publish this sweep's OWN lease \
                 record` pre-flight step (#6320) — without it the in-session dispatch \
                 path publishes no lease record and every in-session claim stays \
                 reclaimable by any host"
            )
        });
    assert!(
        step_1a_pos < step_1b_pos,
        "Step 1a (daemon-claimed: lease already written at dispatch) must precede \
         Step 1b (in-session: publish our own) — Step 1b is defined as the branch \
         Step 1a did NOT take, so stating it first inverts the decision"
    );

    // The peer-lease skip is the safety half: exit 4 means a LIVE peer holds
    // the claim, and publishing over it would hide that worker from every
    // freshest-wins reader. PROSE (not fenced) — a plain bullet sentence.
    assert!(
        content.contains("Exit `4`") || content.contains("exit `4`"),
        "Step 1b must document `sweep-lease-publish.sh`'s exit 4 (a different \
         host holds a fresh lease) as a pre-flight SKIP for that issue"
    );

    // #7950/#7993: EXTRACT the Step 1b fence and EXECUTE it against stub
    // `sweep-lease-publish.sh`/`sweep-lease-renew.sh` scripts, rather than
    // pinning one spelling of the flag-threading. This used to pin the
    // literal `--host "$1" --sweep-id "$2"`; #7876 fixed a real zsh
    // word-splitting bug by switching to named variables and the OLD
    // assertion broke on the CORRECTED doc, red-mining `main` for 10 commits.
    // What must hold is BEHAVIOR: the `start` invocation is threaded with
    // whatever host/sweep-id `publish` returned. Any variable names satisfy
    // that; only dropping the threading (or the exit-4 skip) should fail.
    let (fence, _) = first_fenced_block_after(
        content,
        step_1b_pos,
        "Step 1b — publish this sweep's OWN lease record",
    );
    assert!(
        fence.contains("sweep-lease-publish.sh") && fence.contains("sweep-lease-renew.sh start"),
        "Step 1b's fence must invoke both `sweep-lease-publish.sh` and \
         `sweep-lease-renew.sh start` (#6320). Fence body:\n{fence}"
    );
    assert!(
        fence.contains("$RUN_ID"),
        "Step 1b must key the lease on Step 0a's stable `$RUN_ID`, so the \
         lease, the run registry, and the checkpoints all name one identity. \
         Fence body:\n{fence}"
    );

    let sandbox = tempfile::tempdir().expect("create sandbox dir");
    let scripts_dir = sandbox.path().join(".loom/scripts");
    fs::create_dir_all(&scripts_dir).expect("create .loom/scripts");
    let publish_log = sandbox.path().join("publish-calls.log");
    let renew_log = sandbox.path().join("renew-calls.log");

    // publish stub: behavior controlled by env vars STUB_PUBLISH_RC (exit
    // code) and STUB_PUBLISH_OUT (stdout, the "<host> <sweep-id>" pair).
    write_executable_stub(
        &scripts_dir.join("sweep-lease-publish.sh"),
        &format!(
            "echo \"$@\" >> {publish_log:?}\n\
             [ -n \"${{STUB_PUBLISH_OUT:-}}\" ] && echo \"$STUB_PUBLISH_OUT\"\n\
             exit \"${{STUB_PUBLISH_RC:-0}}\"\n"
        ),
    );
    write_executable_stub(
        &scripts_dir.join("sweep-lease-renew.sh"),
        &format!("echo \"$@\" >> {renew_log:?}\n"),
    );

    let run = |rc: &str, out: &str| -> (String, String) {
        let _ = fs::remove_file(&publish_log);
        let _ = fs::remove_file(&renew_log);
        let output = Command::new("bash")
            .arg("-c")
            .arg(fence)
            .current_dir(sandbox.path())
            .env("N", "42")
            .env("RUN_ID", "sweep-run-abc")
            .env("PPID", "9999")
            .env("STUB_PUBLISH_RC", rc)
            .env("STUB_PUBLISH_OUT", out)
            .output()
            .expect("run extracted Step 1b fence");
        assert!(
            output.status.success(),
            "Step 1b's extracted fence exited non-zero (stderr: {})",
            String::from_utf8_lossy(&output.stderr)
        );
        (
            fs::read_to_string(&publish_log).unwrap_or_default(),
            fs::read_to_string(&renew_log).unwrap_or_default(),
        )
    };

    // Exit 0, a lease identity returned: renew must be started, carrying the
    // EXACT host/sweep-id `publish` returned — this is the #7876 regression
    // surface, now checked by running the real fence instead of pinning text.
    let (publish_calls, renew_calls) = run("0", "host-a sweep-123");
    assert!(
        publish_calls.contains("publish")
            && publish_calls.contains("42")
            && publish_calls.contains("sweep-run-abc"),
        "sweep-lease-publish.sh must be called with `publish 42 --sweep-id sweep-run-abc`; got: {publish_calls:?}"
    );
    assert!(
        renew_calls.contains("start")
            && renew_calls.contains("42")
            && renew_calls.contains("--host")
            && renew_calls.contains("host-a")
            && renew_calls.contains("--sweep-id")
            && renew_calls.contains("sweep-123"),
        "on a successful publish, Step 1b's fence must start renewal pinned to \
         the EXACT host/sweep-id `publish` returned (#6320) — under renewal's \
         default 'newest lease wins' a peer's later lease comment would \
         otherwise be the one kept alive. Got: {renew_calls:?}"
    );

    // Exit 4: a live peer holds a fresher lease — must SKIP, never start renewal.
    let (_, renew_calls) = run("4", "");
    assert!(
        renew_calls.is_empty(),
        "exit 4 (a live peer holds the lease) must NOT start renewal — got: {renew_calls:?}"
    );

    // Exit 2 (best-effort write failure, no identity returned): proceed
    // without a lease — must NOT start renewal on an empty identity.
    let (_, renew_calls) = run("2", "");
    assert!(
        renew_calls.is_empty(),
        "a best-effort publish failure with no lease identity must NOT start \
         renewal (nothing to pin it to) — got: {renew_calls:?}"
    );
}
