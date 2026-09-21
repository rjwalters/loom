//! Owned Rust launch/tool contracts. Real provider canaries are separate evidence.
#![cfg(feature = "otlp")]
use loom_daemon::{
    observability::{lifecycle, queue::DurableQueue},
    telemetry::{
        trace::{store::TraceStore, SpanName, SpanStatus, TraceContext},
        TelemetryRecord,
    },
};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

fn fixture() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let bin = dir.join("harness");
        assert!(Command::new("rustc")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/worker_cli.rs"))
            .arg("-o")
            .arg(&bin)
            .status()
            .unwrap()
            .success());
        bin
    })
}
fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".loom")).unwrap();
    fs::write(dir.path().join(".loom/config.json"), json!({"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}).to_string()).unwrap();
    dir
}
fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env("LOOM_OBSERVABILITY_ENABLED", "true")
        .env("LOOM_OBSERVABILITY_EXPORTER", "otlp")
        .env("LOOM_OBSERVABILITY_ENDPOINT", "http://127.0.0.1:4318")
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        .env("FIXTURE_VERSION", "1.18.31")
        .env("FIXTURE_PRINT_ENV", "LOOM_TRACEPARENT,LOOM_TRACE_CONTEXT_FILE")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        );
    cmd
}
fn spans(root: &Path) -> Vec<loom_daemon::telemetry::trace::SpanRecord> {
    let queue = DurableQueue::open(root.join("queue.jsonl"), 1000);
    lifecycle::backfill(root, &queue);
    queue
        .peek_batch(1000)
        .into_iter()
        .filter_map(|e| match e.record {
            TelemetryRecord::Span(span) => Some(span),
            _ => None,
        })
        .collect()
}

#[test]
fn pi_and_opencode_launch_and_native_read_preserve_authoritative_context() {
    for runtime in ["pi", "opencode"] {
        let dir = root();
        let root = dir.path();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(root)
            .status()
            .unwrap()
            .success());
        let sweep = lifecycle::begin(
            root,
            "fixture-sweep",
            SpanName::Sweep,
            lifecycle::attributes(&[("loom.issue", "18")]),
        )
        .unwrap();
        let mut cmd = command(root);
        cmd.args(["spawn-worker", "--", "-p", "PRIVATE_PROMPT_SENTINEL"])
            .env("LOOM_RUNTIME", runtime);
        sweep.command(&mut cmd);
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8(output.stdout).unwrap();
        let inherited = text
            .lines()
            .find_map(|l| l.strip_prefix("child_env LOOM_TRACEPARENT="))
            .unwrap();
        let context = TraceContext::parse(inherited).unwrap();
        assert_eq!(context.trace_id, sweep.context().trace_id);
        assert_ne!(context.span_id, sweep.context().span_id);
        fs::write(root.join("sample.txt"), "PRIVATE_SOURCE_SENTINEL").unwrap();
        let mut tool = command(root);
        tool.args(["runtime-tool", "--workspace"])
            .arg(root)
            .arg("--cwd")
            .arg(root)
            .env("LOOM_TRACEPARENT", inherited)
            .env("LOOM_TRACE_CONTEXT_FILE", TraceStore::new(root).path(root, "fixture-sweep"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = tool.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"tool":"read","input":{"path":"sample.txt"}})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        lifecycle::finish_execution(root, "fixture-sweep", "success", Default::default());
        let records = spans(root);
        assert_eq!(records.len(), 4, "{records:?}");
        let run = records
            .iter()
            .find(|s| s.name == SpanName::RuntimeRun)
            .unwrap();
        assert_eq!(run.context, context);
        assert_eq!(run.attributes["loom.runtime"], runtime);
        assert_eq!(run.attributes["loom.model"], "glm-5.3-flash");
        let tool = records.iter().find(|s| s.name == SpanName::Tool).unwrap();
        assert_eq!(tool.parent_span_id.as_ref(), Some(&context.span_id));
        assert_eq!(tool.attributes["loom.tool.name"], "read");
        let bytes = serde_json::to_string(&records).unwrap();
        assert!(!bytes.contains("PRIVATE_PROMPT_SENTINEL"));
        assert!(!bytes.contains("PRIVATE_SOURCE_SENTINEL"));
        assert!(
            !TraceStore::new(root).path(root, "fixture-sweep").exists(),
            "completed context retires only after durable backfill"
        );
    }
}

#[test]
fn rejected_preflight_has_no_runtime_run_and_distinct_attempt_failure() {
    let dir = root();
    let root = dir.path();
    let roles = root.join(".loom/roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(roles.join("builder.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    let sweep = lifecycle::begin(root, "rejection", SpanName::Sweep, Default::default()).unwrap();
    let mut cmd = command(root);
    cmd.args(["spawn-worker", "--", "-p", "/loom:builder 18"])
        .env("LOOM_RUNTIME", "pi");
    sweep.command(&mut cmd);
    assert_eq!(cmd.output().unwrap().status.code(), Some(78));
    lifecycle::finish_execution(root, "rejection", "failure", Default::default());
    let records = spans(root);
    assert!(!records.iter().any(|s| s.name == SpanName::RuntimeRun));
    assert!(records
        .iter()
        .any(|s| s.name == SpanName::RuntimePreflight && s.status == SpanStatus::Error));
    assert!(records
        .iter()
        .any(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.result"] == "rejected"));
}

#[test]
fn identical_issue_numbers_in_distinct_roots_keep_distinct_trace_ids_after_reload() {
    let a = root();
    let b = root();
    let one = lifecycle::begin(a.path(), "issue-18", SpanName::Sweep, Default::default()).unwrap();
    let two = lifecycle::begin(b.path(), "issue-18", SpanName::Sweep, Default::default()).unwrap();
    assert_ne!(one.context().trace_id, two.context().trace_id);
    let restored =
        lifecycle::begin(a.path(), "issue-18", SpanName::Sweep, Default::default()).unwrap();
    assert_eq!(one.context(), restored.context());
    lifecycle::finish_execution(a.path(), "issue-18", "cancelled", Default::default());
    lifecycle::finish_execution(b.path(), "issue-18", "crashed", Default::default());
    assert_eq!(spans(a.path())[0].attributes["loom.result"], "cancelled");
    assert_eq!(spans(b.path())[0].attributes["loom.result"], "crashed");
}

#[test]
fn actual_checkpoint_cli_preserves_rapid_judge_doctor_repair_waterfall() {
    let dir = root();
    let root = dir.path();
    let sweep = lifecycle::begin(root, "repair", SpanName::Sweep, Default::default()).unwrap();
    for (phase, attempt) in [
        ("builder-done", "1"),
        ("judge-rejected", "1"),
        ("doctor-done", "2"),
        ("judge-done", "2"),
        ("merge-done", "2"),
    ] {
        let mut cmd = command(root);
        cmd.args([
            "sweep-checkpoint",
            "write",
            "18",
            phase,
            "--pr-number",
            "42",
            "--attempt",
            attempt,
        ]);
        sweep.command(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    lifecycle::finish_execution(root, "repair", "success", Default::default());
    let records = spans(root);
    assert_eq!(records.len(), 11);
    let judges: Vec<_> = records
        .iter()
        .filter(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.role"] == "judge")
        .collect();
    assert_eq!(judges.len(), 2);
    assert_eq!(judges[0].status, SpanStatus::Error);
    assert_eq!(judges[1].status, SpanStatus::Ok);
    assert_ne!(judges[0].context.span_id, judges[1].context.span_id);
    assert!(judges.iter().all(|s| s.started_at == s.ended_at
        && s.attributes["loom.timing_source"] == "checkpoint_write_observed"));
}
