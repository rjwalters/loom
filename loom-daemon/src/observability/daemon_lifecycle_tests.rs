#![allow(clippy::unwrap_used)]

use super::*;
use crate::observability::exporter::{ExportError, Exporter};
use serial_test::serial;

fn payload(record: &TelemetryRecord) -> (&str, &serde_json::Value) {
    let TelemetryRecord::DaemonEvent(inner) = record else {
        panic!("expected a daemon.event record, got {record:?}");
    };
    (inner.topic.as_str(), &inner.payload)
}

#[test]
fn start_record_names_version_build_commit_and_supervisor() {
    let record = start_record("launchd");
    let (topic, p) = payload(&record);
    assert_eq!(topic, "daemon.start");
    assert_eq!(p["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(p["build_commit"], crate::self_update::BUILT_COMMIT_FULL);
    assert_eq!(p["supervisor"], "launchd");
    assert_eq!(p["pid"], std::process::id());
}

#[test]
fn shutdown_record_is_clean_and_names_its_exit() {
    let record = shutdown_record(crate::ipc::EXIT_SIGINT, Duration::from_secs(90));
    let (topic, p) = payload(&record);
    assert_eq!(topic, "daemon.shutdown");
    assert_eq!(p["exit_code"], 130);
    assert_eq!(p["reason"], "sigint");
    assert_eq!(p["clean"], true);
    assert_eq!(p["uptime_sec"], 90);
    assert_eq!(exit_reason(crate::ipc::EXIT_RESTART), "restart");
    assert_eq!(exit_reason(crate::ipc::EXIT_SHUTDOWN), "stop");
    assert_eq!(exit_reason(crate::fleet_state::EXIT_FLEET_STOPPED), "fleet_stopped");
    assert_eq!(exit_reason(42), "exit");
}

fn envelope(topic: &str) -> TelemetryEnvelope {
    TelemetryEnvelope::new("h", record(topic, serde_json::json!({})))
}

/// AC: the heartbeat carries export health from the existing status/queue —
/// `last_success_at`, queued and dropped — per exporter and aggregated.
#[test]
fn heartbeat_carries_export_health_from_status_and_queue() {
    let dir = tempfile::tempdir().unwrap();
    let healthy_queue = Arc::new(DurableQueue::open(dir.path().join("a.jsonl"), 2));
    let failing_queue = Arc::new(DurableQueue::open(dir.path().join("b.jsonl"), 2));
    // Three pushes into capacity 2: one queued record dropped.
    for _ in 0..3 {
        failing_queue.push(envelope("x"));
    }
    healthy_queue.push(envelope("y"));
    let healthy = Arc::new(ExportStatus::started("h", "http://127.0.0.1:4318", "otlp", 30));
    healthy.record_success(5);
    let failing = Arc::new(ExportStatus::started("h", "https://ingest.invalid", "https", 30));
    failing.record_failure("connection refused");
    let sources = vec![
        ExportHealthSource {
            name: "otlp".into(),
            queue: healthy_queue,
            status: healthy.clone(),
        },
        ExportHealthSource {
            name: "https".into(),
            queue: failing_queue,
            status: failing,
        },
    ];

    let record = heartbeat_record(&sources, Duration::from_secs(600));
    let (topic, p) = payload(&record);
    assert_eq!(topic, "daemon.heartbeat");
    assert_eq!(p["uptime_sec"], 600);
    assert_eq!(p["queued"], 3, "1 + 2 queued");
    assert_eq!(p["dropped"], 1);
    assert_eq!(
        p["last_success_at"],
        serde_json::to_value(healthy.snapshot().last_success_at).unwrap()
    );
    assert!(p["last_failure_at"].is_string());
    let exports = p["exports"].as_array().unwrap();
    assert_eq!(exports.len(), 2);
    assert_eq!(exports[1]["exporter"], "https");
    assert!(exports[1]["last_success_at"].is_null(), "never succeeded ⇒ null, not a guess");
    assert_eq!(exports[1]["consecutive_failures"], 1);
    assert_eq!(exports[1]["queued"], 2);
    assert_eq!(exports[1]["dropped"], 1);
}

#[test]
fn heartbeat_with_no_success_yet_reports_null_not_zero() {
    let record = heartbeat_record(&[], Duration::from_secs(1));
    let (_, p) = payload(&record);
    assert!(p["last_success_at"].is_null());
    assert_eq!(p["queued"], 0);
}

#[test]
#[serial(loom_config_env)]
fn heartbeat_interval_is_env_then_config_then_default() {
    let saved_env = std::env::var(HEARTBEAT_SECS_ENV).ok();
    let saved_defaults = std::env::var(crate::config_resolver::PRIVATE_DEFAULTS_ENV).ok();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    std::env::remove_var(HEARTBEAT_SECS_ENV);
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_heartbeat_interval(dir.path()),
        Duration::from_secs(DEFAULT_HEARTBEAT_SECS)
    );
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom").join("config.json"),
        r#"{"observability": {"heartbeatSecs": 60}}"#,
    )
    .unwrap();
    assert_eq!(resolve_heartbeat_interval(dir.path()), Duration::from_secs(60));
    std::env::set_var(HEARTBEAT_SECS_ENV, "300");
    assert_eq!(resolve_heartbeat_interval(dir.path()), Duration::from_secs(300));
    std::env::set_var(HEARTBEAT_SECS_ENV, "0");
    assert_eq!(
        resolve_heartbeat_interval(dir.path()),
        Duration::from_secs(60),
        "0 falls through"
    );
    match saved_env {
        Some(v) => std::env::set_var(HEARTBEAT_SECS_ENV, v),
        None => std::env::remove_var(HEARTBEAT_SECS_ENV),
    }
    match saved_defaults {
        Some(v) => std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, v),
        None => std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV),
    }
}

/// Records every exported envelope's topic, in export order.
struct Capture(Arc<Mutex<Vec<String>>>);

impl Exporter for Capture {
    async fn emit_batch(&self, batch: &[TelemetryEnvelope]) -> Result<(), ExportError> {
        let mut seen = self.0.lock().unwrap();
        for envelope in batch {
            if let TelemetryRecord::DaemonEvent(r) = &envelope.record {
                seen.push(r.topic.clone());
            }
        }
        Ok(())
    }
}

/// AC: on clean shutdown `daemon.shutdown` is enqueued and flushed before
/// exit — the final drain exports it, after `daemon.start`, as the last
/// record; and a second exit path cannot enqueue a second one.
#[tokio::test]
#[serial]
async fn shutdown_record_is_enqueued_before_and_exported_by_the_final_drain() {
    let dir = tempfile::tempdir().unwrap();
    let queue = Arc::new(DurableQueue::open(dir.path().join("q.jsonl"), 50));
    let seen = Arc::new(Mutex::new(Vec::new()));
    start(queue.clone(), "host-under-test".into(), Instant::now());
    // A long interval: the sender is asleep, so only the final drain can
    // export the shutdown record.
    let task = super::super::shutdown::spawn_sender(
        queue.clone(),
        Capture(seen.clone()),
        50,
        Duration::from_secs(3600),
        Arc::new(ExportStatus::started("host-under-test", "http://localhost", "otlp", 3600)),
    );
    // Let the sender take its first (start-record) batch.
    for _ in 0..200 {
        if !seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    super::super::shutdown::finish(crate::ipc::EXIT_SHUTDOWN, Duration::from_secs(5)).await;
    assert!(!record_shutdown(0), "the sink is consumed: no second shutdown record");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.first().map(String::as_str), Some(TOPIC_START), "{seen:?}");
    assert_eq!(seen.last().map(String::as_str), Some(TOPIC_SHUTDOWN), "{seen:?}");
    assert_eq!(seen.iter().filter(|t| *t == TOPIC_SHUTDOWN).count(), 1);
    assert!(queue.is_empty(), "the drain delivered everything");
    task.abort();
}

/// The gateway's log allowlist must keep every typed lifecycle attribute —
/// an attribute the collector drops is one no SigNoz query can read.
#[test]
fn collector_keeps_every_lifecycle_log_attribute() {
    const CONFIG: &str = include_str!("../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.topic"))
        .expect("the transform/privacy log keep_keys line");
    for key in [
        "loom.kind",
        "loom.topic",
        "loom.daemon.version",
        "loom.daemon.revision",
        "loom.daemon.tree_state",
        "loom.daemon.supervisor",
        "loom.daemon.exit_code",
        "loom.daemon.exit_reason",
        "loom.daemon.uptime_sec",
        "loom.export.last_success_at",
        "loom.export.last_failure_at",
        "loom.export.queued",
        "loom.export.dropped",
    ] {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}
