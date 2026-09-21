//! Opt-in real Collector contract: cargo test -p loom-daemon --test collector_fanout -- --ignored --nocapture
//! Needs Docker, curl and the pinned image. Owns uniquely named containers/network.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct TempDir(PathBuf);
impl TempDir {
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn unique_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
fn tempdir() -> TempDir {
    let path = std::env::temp_dir().join(format!("loom-fanout-{}", unique_id()));
    fs::create_dir(&path).unwrap();
    TempDir(path)
}

const IMAGE: &str = "otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1";
const CONFIG: &str = include_str!("../../defaults/observability/collector/config.yaml");

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .expect("Docker required");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Trial {
    dir: TempDir,
    network: String,
    containers: Vec<String>,
}
impl Drop for Trial {
    fn drop(&mut self) {
        if std::thread::panicking() {
            for name in &self.containers {
                if let Ok(out) = Command::new("docker")
                    .args(["logs", "--tail", "40", name])
                    .output()
                {
                    eprintln!(
                        "{name}: {}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
        }
        for name in &self.containers {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
    }
}
impl Trial {
    fn new() -> Self {
        let trial = Self {
            dir: tempdir(),
            network: format!("loom-fanout-{}", unique_id()),
            containers: vec![],
        };
        docker(&["network", "create", &trial.network]);
        for (name, value) in [
            ("loom_ingest_key", "fixture-loom-key"),
            ("clickstack_ingest_key", "fixture-clickstack-key"),
        ] {
            fs::write(trial.dir.path().join(name), value).unwrap();
        }
        trial
    }
    fn start(&mut self, suffix: &str, config: &str, alias: &str) -> String {
        let name = format!("{}-{suffix}", self.network);
        let config_file = self.dir.path().join(format!("{suffix}.yaml"));
        fs::write(&config_file, config).unwrap();
        let data = self.dir.path().join(suffix);
        fs::create_dir_all(&data).unwrap();
        self.containers.push(name.clone());
        docker(&[
            "run",
            "-d",
            "--name",
            &name,
            "--network",
            &self.network,
            "--network-alias",
            alias,
            "--user",
            "0:0",
            "--memory",
            "256m",
            "--shm-size",
            "1m",
            "-p",
            "127.0.0.1::4318",
            "-p",
            "127.0.0.1::8888",
            "-v",
            &format!("{}:/etc/config.yaml:ro", config_file.display()),
            "-v",
            &format!("{}:/run/secrets:ro", self.dir.path().display()),
            "-v",
            &format!("{}:/var/lib/otelcol", data.display()),
            IMAGE,
            "--config=/etc/config.yaml",
        ]);
        name
    }
    fn endpoint(&self, name: &str, port: &str) -> String {
        format!("http://{}", docker(&["port", name, port]))
    }
    fn contents(&self, sink: &str) -> String {
        fs::read_to_string(self.dir.path().join(sink).join("data.json")).unwrap_or_default()
    }
    fn wait_for(&self, sink: &str, needle: &str) {
        for _ in 0..300 {
            if self.contents(sink).contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        for container in &self.containers {
            eprintln!("{}", docker(&["logs", container]));
        }
        panic!("{sink} did not receive {needle}: {}", self.contents(sink));
    }
}

fn sink_config(auth: bool) -> String {
    let auth_extensions = if auth {
        "  bearertokenauth/backend:\n    scheme: \"\"\n    token: fixture-clickstack-key\n"
    } else {
        ""
    };
    let auth_receiver = if auth {
        "        auth:\n          authenticator: bearertokenauth/backend\n"
    } else {
        ""
    };
    format!("extensions:\n  health_check: {{}}\n{auth_extensions}receivers:\n  otlp:\n    protocols:\n      http:\n        endpoint: 0.0.0.0:4318\n{auth_receiver}exporters:\n  file:\n    path: /var/lib/otelcol/data.json\n    flush_interval: 100ms\nservice:\n  extensions: [{}]\n  pipelines:\n    logs:\n      receivers: [otlp]\n      exporters: [file]\n    metrics:\n      receivers: [otlp]\n      exporters: [file]\n    traces:\n      receivers: [otlp]\n      exporters: [file]\n", if auth { "health_check, bearertokenauth/backend" } else { "health_check" })
}

fn fixture(signal: &str, id: &str) -> String {
    // All call sites supply fixed ASCII IDs or repeated 'x', never user strings.
    assert!(id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    let resource = r#"{"attributes":[{"key":"service.name","value":{"stringValue":"loom-daemon"}},{"key":"host.id","value":{"stringValue":"fixture-host"}},{"key":"secret","value":{"stringValue":"MUST-NOT-EXPORT"}}]}"#;
    let attrs = format!(
        r#"[{{"key":"loom.sweep_id","value":{{"stringValue":"{id}"}}}},{{"key":"loom.repo.visibility","value":{{"stringValue":"private"}}}},{{"key":"authorization","value":{{"stringValue":"MUST-NOT-EXPORT"}}}},{{"key":"loom.detail","value":{{"stringValue":"MUST-NOT-EXPORT"}}}}]"#
    );
    let identity = r#""traceId":"00112233445566778899aabbccddeeff","spanId":"0011223344556677""#;
    let (resource_key, scope_key, records_key, record) = match signal {
        "logs" => (
            "resourceLogs",
            "scopeLogs",
            "logRecords",
            format!(
                r#"{{"timeUnixNano":"1790000000000000000","body":{{"stringValue":"{id}"}},{identity},"attributes":{attrs}}}"#
            ),
        ),
        "traces" => (
            "resourceSpans",
            "scopeSpans",
            "spans",
            format!(
                r#"{{{identity},"name":"{id}","startTimeUnixNano":"1790000000000000000","endTimeUnixNano":"1790000001000000000","attributes":{attrs}}}"#
            ),
        ),
        "metrics" => (
            "resourceMetrics",
            "scopeMetrics",
            "metrics",
            format!(
                r#"{{"name":"{id}","gauge":{{"dataPoints":[{{"timeUnixNano":"1790000000000000000","asDouble":42.5,"attributes":{attrs}}}]}}}}"#
            ),
        ),
        _ => panic!("unknown signal"),
    };
    format!(
        r#"{{"{resource_key}":[{{"resource":{resource},"{scope_key}":[{{"{records_key}":[{record}]}}]}}]}}"#
    )
}
struct Response {
    status: u16,
    body: String,
}
impl Response {
    fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}
fn http(url: &str, body: Option<&str>, key: &str) -> Response {
    let mut command = Command::new("curl");
    command.args([
        "--silent",
        "--show-error",
        "--max-time",
        "10",
        "--write-out",
        "\\n%{http_code}",
        url,
    ]);
    if body.is_some() {
        command.args([
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("Authorization: Bearer {key}"),
            "--data-binary",
            "@-",
        ]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("curl required");
    if let Some(body) = body {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", "0"));
    Response {
        status: status.trim().parse().unwrap_or(0),
        body: body.to_owned(),
    }
}
fn send(endpoint: &str, signal: &str, id: &str, key: &str) -> Response {
    http(&format!("{endpoint}/v1/{signal}"), Some(&fixture(signal, id)), key)
}
fn ready(endpoint: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while std::time::Instant::now() < deadline {
        if http(&format!("{endpoint}/v1/logs"), None, "").status != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("collector did not start at {endpoint}");
}

#[test]
#[ignore = "requires Docker; starts three isolated pinned Collector containers"]
fn three_signals_fan_out_auth_redact_and_survive_outage_restart() {
    let mut trial = Trial::new();
    let a = trial.start("clickstack", &sink_config(true), "clickstack-collector");
    let b = trial.start("signoz", &sink_config(false), "signoz-otel-collector");
    // Same production config; smaller memory budget for this low-volume fixture.
    let config = CONFIG
        .replace("limit_mib: 384", "limit_mib: 160")
        .replace("spike_limit_mib: 96", "spike_limit_mib: 32");
    ready(&trial.endpoint(&a, "4318"));
    ready(&trial.endpoint(&b, "4318"));
    let gateway = trial.start("gateway", &config, "gateway");
    let endpoint = trial.endpoint(&gateway, "4318");
    ready(&endpoint);
    assert_eq!(send(&endpoint, "logs", "unauthorized-record", "wrong-key").status, 401);
    let rejected = send(&endpoint, "logs", "rejected", "reflected-secret-canary").body;
    assert!(!rejected.contains("reflected-secret-canary"));
    for signal in ["logs", "metrics", "traces"] {
        let id = format!("baseline-{signal}");
        assert!(send(&endpoint, signal, &id, "fixture-loom-key").success());
        trial.wait_for("clickstack", &id);
        trial.wait_for("signoz", &id);
    }
    for sink in ["clickstack", "signoz"] {
        let data = trial.contents(sink);
        assert!(!data.contains("MUST-NOT-EXPORT"));
        assert!(!data.contains("unauthorized-record"));
        assert!(!data.contains("fixture-loom-key"));
        assert!(data.contains("00112233445566778899aabbccddeeff"));
        assert!(data.contains("42.5"));
        assert!(data.contains("1790000000000000000"));
    }
    for (down, down_suffix, healthy_suffix) in
        [(&a, "clickstack", "signoz"), (&b, "signoz", "clickstack")]
    {
        docker(&["stop", "-t", "2", down]);
        for signal in ["logs", "metrics", "traces"] {
            let id = format!("outage-{down_suffix}-{signal}");
            assert!(send(&endpoint, signal, &id, "fixture-loom-key").success());
            trial.wait_for(healthy_suffix, &id);
        }
        // Abrupt termination exercises persistent queues, not graceful draining.
        docker(&["kill", &gateway]);
        docker(&["start", &gateway]);
        ready(&endpoint);
        docker(&["start", down]);
        for signal in ["logs", "metrics", "traces"] {
            trial.wait_for(down_suffix, &format!("outage-{down_suffix}-{signal}"));
        }
    }
    let metrics = http(&format!("{}/metrics", trial.endpoint(&gateway, "8888")), None, "").body;
    assert!(metrics.contains("otelcol_exporter_sent_spans"));
    assert!(metrics.contains("otlp_http/clickstack"));
    assert!(metrics.contains("otlp_http/signoz"));
}

fn positive_counter(text: &str, prefix: &str) -> bool {
    text.lines()
        .filter(|line| line.starts_with(prefix) && line.contains("otlp_http/clickstack"))
        .any(|line| {
            line.split_whitespace()
                .last()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|v| v > 0.0)
        })
}

fn wait_counter(trial: &Trial, gateway: &str, prefix: &str) {
    let endpoint = format!("{}/metrics", trial.endpoint(gateway, "8888"));
    for _ in 0..300 {
        let text = http(&endpoint, None, "").body;
        if positive_counter(&text, prefix) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("missing positive collector counter {prefix}");
}

#[test]
#[ignore = "requires Docker; isolated queue/storage/credential fault injections"]
fn queue_retry_storage_and_credentials_fail_visibly() {
    let mut trial = Trial::new();
    let config = CONFIG
        .replace("limit_mib: 384", "limit_mib: 160")
        .replace("spike_limit_mib: 96", "spike_limit_mib: 32")
        .replace("queue_size: 1000", "queue_size: 2")
        .replace("max_elapsed_time: 600s", "max_elapsed_time: 2s")
        .replace("max_interval: 10s", "max_interval: 1s");
    // No destinations exist: capacity is finite, then retry deadlines expire.
    let gateway = trial.start("faults", &config, "gateway");
    let endpoint = trial.endpoint(&gateway, "4318");
    ready(&endpoint);
    for n in 0..20 {
        let _ = send(&endpoint, "logs", &format!("overflow-{n}"), "fixture-loom-key");
    }
    wait_counter(&trial, &gateway, "otelcol_exporter_enqueue_failed_log_records");
    wait_counter(&trial, &gateway, "otelcol_exporter_send_failed_log_records");
    std::thread::sleep(Duration::from_secs(3));
    let retry_logs = Command::new("docker")
        .args(["logs", &gateway])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&retry_logs.stderr)
            .to_lowercase()
            .contains("dropping data"),
        "retry expiration must be reported as discarded data"
    );
    docker(&["stop", "-t", "2", &gateway]);

    // Permanent backend 401 must show a send failure, not remain silently healthy.
    let sink = trial.start("authsink", &sink_config(true), "clickstack-collector");
    ready(&trial.endpoint(&sink, "4318"));
    fs::write(trial.dir.path().join("clickstack_ingest_key"), "wrong-backend-key").unwrap();
    let auth_gateway = trial.start("authfault", &config, "auth-gateway");
    let endpoint = trial.endpoint(&auth_gateway, "4318");
    ready(&endpoint);
    assert!(send(&endpoint, "logs", "rejected-by-backend", "fixture-loom-key").success());
    wait_counter(&trial, &auth_gateway, "otelcol_exporter_send_failed_log_records");
    assert!(!trial.contents("authsink").contains("rejected-by-backend"));
    docker(&["stop", "-t", "2", &auth_gateway]);

    // A 1-MiB private tmpfs fills before the 1000-request limit. This exercises
    // actual persistent-store write errors without filling the host filesystem.
    let disk_config = CONFIG
        .replace("/var/lib/otelcol", "/dev/shm")
        .replace("limit_mib: 384", "limit_mib: 160")
        .replace("spike_limit_mib: 96", "spike_limit_mib: 32")
        .replace("http://clickstack-collector:4318", "http://missing-clickstack:4318");
    let disk_gateway = trial.start("diskfault", &disk_config, "disk-gateway");
    let endpoint = trial.endpoint(&disk_gateway, "4318");
    ready(&endpoint);
    let body = "x".repeat(65536);
    for _ in 0..50 {
        let _ = send(&endpoint, "logs", &body, "fixture-loom-key");
    }
    wait_counter(&trial, &disk_gateway, "otelcol_exporter_enqueue_failed_log_records");
    // Logs must never dump ingress/egress key values even while reporting faults.
    for name in &trial.containers {
        let out = Command::new("docker")
            .args(["logs", name])
            .output()
            .unwrap();
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!logs.contains("fixture-loom-key"));
        assert!(!logs.contains("wrong-backend-key"));
    }
}
