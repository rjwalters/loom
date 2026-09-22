//! Explicit integration test: requires Docker, never converts missing Docker to a pass.
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const IMAGE: &str = "otel/opentelemetry-collector-contrib:0.139.0@sha256:faf125d656fa47cea568b2f3b4494efd2525083bc75c1e96038bc23f05cd68fd";
struct Collector(String);
impl Drop for Collector {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}
fn command_ok(command: &mut Command) -> String {
    let output = command.output().expect("required executable unavailable");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn attributes(values: &serde_json::Value) -> bool {
    values.as_array().is_some_and(|a| {
        a.iter()
            .any(|kv| kv["key"] == "host.id" && kv["value"]["stringValue"] == "loom-otlp-canary")
    })
}

fn receiver_ready(address: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect(address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    if stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut bytes = [0; 64];
    stream
        .read(&mut bytes)
        .is_ok_and(|n| n >= 5 && &bytes[..5] == b"HTTP/")
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn real_collector_decodes_installed_binary_logs_metrics_and_correlated_traces() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/otlp_transport");
    let evidence = tempfile::tempdir().unwrap();
    let collector = Collector(format!("loom-8523-{}", std::process::id()));
    command_ok(
        Command::new("docker")
            .args([
                "run",
                "--detach",
                "--name",
                &collector.0,
                "--user",
                "0",
                "--publish",
                "127.0.0.1::4318",
                "--volume",
            ])
            .arg(format!(
                "{}:/etc/otelcol-contrib/config.yaml:ro",
                fixture.join("collector.yaml").display()
            ))
            .arg("--volume")
            .arg(format!("{}:/evidence", evidence.path().display()))
            .arg(IMAGE),
    );
    let port = command_ok(Command::new("docker").args(["port", &collector.0, "4318/tcp"]));
    let address = port.trim();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if receiver_ready(address) {
            break;
        }
        assert!(Instant::now() < deadline, "collector startup timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
    let binary = std::env::var_os("LOOM_OTLP_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_loom-daemon").into());
    command_ok(Command::new(&binary).args(["telemetry-capabilities", "--require-otlp"]));
    let key = evidence.path().join("key");
    std::fs::write(&key, "synthetic-test-key").unwrap();
    let ack = command_ok(
        Command::new(&binary)
            .args(["telemetry-export", "--input"])
            .arg(fixture.join("envelopes.jsonl"))
            .arg("--endpoint")
            .arg(format!("http://{address}"))
            .arg("--key-file")
            .arg(key),
    );
    let ack: serde_json::Value = serde_json::from_str(&ack).unwrap();
    assert_eq!(ack["exported_envelopes"], 6);
    let deadline = Instant::now() + Duration::from_secs(10);
    let text = loop {
        let text =
            std::fs::read_to_string(evidence.path().join("received.json")).unwrap_or_default();
        if text.contains("resourceLogs")
            && text.contains("resourceMetrics")
            && text.contains("resourceSpans")
        {
            break text;
        }
        assert!(Instant::now() < deadline, "collector never decoded all three signals: {text}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let decoded: Vec<serde_json::Value> = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let logs: Vec<_> = decoded
        .iter()
        .filter_map(|v| v["resourceLogs"].as_array())
        .flatten()
        .collect();
    assert!(logs
        .iter()
        .all(|r| attributes(&r["resource"]["attributes"])));
    let records: Vec<_> = logs
        .iter()
        .flat_map(|r| r["scopeLogs"].as_array().unwrap())
        .flat_map(|s| s["logRecords"].as_array().unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["timeUnixNano"], "1789992000000000000");
    assert!(records
        .iter()
        .any(|r| r["eventName"] == "role_tick.outcome"));
    let metrics: Vec<_> = decoded
        .iter()
        .filter_map(|v| v["resourceMetrics"].as_array())
        .flatten()
        .flat_map(|r| r["scopeMetrics"].as_array().unwrap())
        .flat_map(|s| s["metrics"].as_array().unwrap())
        .collect();
    let uptime = metrics
        .iter()
        .find(|m| m["name"] == "loom.host.uptime_seconds")
        .expect("missing host uptime");
    assert_eq!(uptime["gauge"]["dataPoints"][0]["asInt"], "42");
    assert!(metrics
        .iter()
        .any(|m| m["name"].as_str().unwrap().starts_with("loom.tokens.")));
    assert!(!metrics
        .iter()
        .any(|m| m["name"] == "loom.host.cpu_idle_fraction"));
    let spans: Vec<_> = decoded
        .iter()
        .filter_map(|v| v["resourceSpans"].as_array())
        .flatten()
        .flat_map(|r| r["scopeSpans"].as_array().unwrap())
        .flat_map(|s| s["spans"].as_array().unwrap())
        .collect();
    assert_eq!(spans.len(), 2);
    let child = spans
        .iter()
        .find(|s| s["name"] == "loom.role_attempt")
        .unwrap();
    let root = spans.iter().find(|s| s["name"] == "loom.sweep").unwrap();
    assert_eq!(child["parentSpanId"], root["spanId"]);
    assert_eq!(child["traceId"], root["traceId"]);
    let linked_log = records
        .iter()
        .find(|r| r["eventName"] == "sweep.started")
        .unwrap();
    assert_eq!(linked_log["traceId"], child["traceId"]);
    assert_eq!(linked_log["spanId"], child["spanId"]);
    assert!(!text.contains("PRIVACY_SENTINEL"));
    println!(
        "Collector {IMAGE}; independently decoded {} logs and {} metrics; binary={}",
        records.len(),
        metrics.len(),
        binary.to_string_lossy()
    );
}
