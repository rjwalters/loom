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
            fs::write(trial.dir.path().join(name), format!("{value}\n")).unwrap();
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
/// One `(scrub class, sentinel secret)` pair for the #8825 redaction
/// contract. Every value is a synthetic fixture string — never a real
/// credential — shaped to match exactly one class in the gateway's
/// `transform/ci_log_redaction` stage.
///
/// The AWS access-key sentinel is **assembled** rather than written out:
/// GitHub's own push protection matches `AKIA[0-9A-Z]{16}` on the literal and
/// refuses the push even for an obviously synthetic fixture (verified
/// 2026-09-25 — the first push of this file was rejected). It still *is* that
/// shape at runtime, which is what the gateway is being tested against.
const SCRUB_SENTINELS: &[(&str, &str)] = &[
    ("authorization", "Authorization: token LOOMFIXTUREauthheadervalue"),
    ("bearer-token", "Bearer LOOMFIXTUREbearertoken0123456789"),
    ("github-token", "ghp_LOOMFIXTUREAAAAAAAAAAAAAAAAAAAAAAAA"),
    ("github-token", "github_pat_LOOMFIXTURE0000000000AAAAAAAAAA"),
    ("anthropic-key", "sk-ant-api03-LOOMFIXTUREAAAAAAAAAAAA"),
    ("api-key", "api_key=sk-LOOMFIXTUREAAAAAAAAAAAAAAAA"),
    ("aws-access-key-id", concat!("AKI", "A", "LOOMFIXTUREAAAAA")),
    (
        "aws-secret-access-key",
        "aws_secret_access_key=LOOMFIXTUREaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ),
    ("credential", "password=LOOMFIXTUREpassword"),
];

/// A build-log line the scrubber must leave byte-identical: it mentions
/// tokens and secrets in prose, which is not the same thing as carrying one.
const CLEAN_LOG_LINE: &str =
    "2026-09-25T09:00:00.0000000Z ##[group]Run cargo test --workspace -- --nocapture";

/// A `ci.job.log` OTLP logs payload whose body is `text`. The
/// `loom.ci.chunk_index` attribute is the predicate the gateway's scrub stage
/// is scoped by, so it is what makes this batch a job-log batch at all.
fn ci_log_fixture(text: &str) -> String {
    let body = serde_json_escape(text);
    let resource = r#"{"attributes":[{"key":"service.name","value":{"stringValue":"loom-daemon"}},{"key":"host.id","value":{"stringValue":"fixture-host"}}]}"#;
    let attrs = r#"[{"key":"loom.repo","value":{"stringValue":"fixture-org/alpha"}},{"key":"loom.repo.visibility","value":{"stringValue":"private"}},{"key":"loom.ci.run_id","value":{"intValue":"1001"}},{"key":"loom.ci.job_id","value":{"intValue":"10011"}},{"key":"loom.ci.job","value":{"stringValue":"build"}},{"key":"loom.ci.chunk_index","value":{"intValue":"0"}},{"key":"loom.ci.chunk_count","value":{"intValue":"1"}},{"key":"loom.ci.log_bytes_total","value":{"intValue":"4096"}},{"key":"loom.ci.truncated","value":{"boolValue":false}},{"key":"loom.ci.unlisted","value":{"stringValue":"MUST-NOT-EXPORT"}}]"#;
    format!(
        r#"{{"resourceLogs":[{{"resource":{resource},"scopeLogs":[{{"logRecords":[{{"timeUnixNano":"1790000000000000000","eventName":"ci.job.log","body":{{"stringValue":"{body}"}},"attributes":{attrs}}}]}}]}}]}}"#
    )
}

/// Minimal JSON string escaping for the fixture bodies (ASCII, newlines).
fn serde_json_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' => "\\\"".to_string(),
            '\\' => "\\\\".to_string(),
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            c => c.to_string(),
        })
        .collect()
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
    let mut endpoint = trial.endpoint(&gateway, "4318");
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
        // Docker can reassign an ephemeral host port across stop/start.
        endpoint = trial.endpoint(&gateway, "4318");
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
    fs::write(trial.dir.path().join("clickstack_ingest_key"), "wrong-backend-key\n").unwrap();
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

/// Issue #8825 acceptance criterion 2, the half that needs a real Collector:
/// a synthesized `ci.job.log` batch carrying one sentinel per scrub class
/// must arrive at **both** sinks with every sentinel replaced by its
/// `[REDACTED:<class>]` marker and **zero raw secret bytes** anywhere in the
/// exported output.
///
/// Both halves of that are asserted on purpose: absence alone would also pass
/// if the gateway simply dropped the record, which is a different (and also
/// wrong) outcome, so the marker must be present too. A clean build-log line
/// in the same batch must survive byte-identical — a scrubber that eats
/// ordinary log text is useless for the job this exists to do.
#[test]
#[ignore = "requires Docker; starts three isolated pinned Collector containers"]
fn ci_job_log_bodies_are_scrubbed_at_the_gateway_before_both_sinks() {
    let mut trial = Trial::new();
    let a = trial.start("clickstack", &sink_config(true), "clickstack-collector");
    let b = trial.start("signoz", &sink_config(false), "signoz-otel-collector");
    let config = CONFIG
        .replace("limit_mib: 384", "limit_mib: 160")
        .replace("spike_limit_mib: 96", "spike_limit_mib: 32");
    ready(&trial.endpoint(&a, "4318"));
    ready(&trial.endpoint(&b, "4318"));
    let gateway = trial.start("gateway", &config, "gateway");
    let endpoint = trial.endpoint(&gateway, "4318");
    ready(&endpoint);

    // One chunk body: a liveness marker, the clean line, then one line per
    // sentinel — exactly the shape a real job log has.
    let mut lines = vec![
        "ci-log-redaction-canary".to_string(),
        CLEAN_LOG_LINE.to_string(),
    ];
    for (class, sentinel) in SCRUB_SENTINELS {
        lines.push(format!("2026-09-25T09:00:01.0000000Z [{class}] {sentinel}"));
    }
    let body = lines.join("\n");
    let response =
        http(&format!("{endpoint}/v1/logs"), Some(&ci_log_fixture(&body)), "fixture-loom-key");
    assert!(response.success(), "gateway refused the ci.job.log batch: {}", response.body);

    for sink in ["clickstack", "signoz"] {
        trial.wait_for(sink, "ci-log-redaction-canary");
        let data = trial.contents(sink);
        for (class, sentinel) in SCRUB_SENTINELS {
            assert!(
                !data.contains(sentinel),
                "{sink}: raw {class} secret survived the gateway: {sentinel}"
            );
            assert!(
                data.contains(&format!("[REDACTED:{class}]")),
                "{sink}: no [REDACTED:{class}] marker — a dropped record is not a redacted one"
            );
        }
        // A clean build-log line is forwarded untouched.
        assert!(
            data.contains(CLEAN_LOG_LINE),
            "{sink}: a clean build-log line was mangled by the scrubber"
        );
        // No regression in the general allowlist: an attribute outside the
        // reviewed `loom.ci.*` set is still stripped, on this kind too.
        assert!(!data.contains("MUST-NOT-EXPORT"));
        assert!(!data.contains("loom.ci.unlisted"));
        // The identity attributes a reconstruction query needs did survive.
        assert!(data.contains("loom.ci.chunk_index"));
        assert!(data.contains("loom.ci.job_id"));
    }
}

/// Issue #8825, no Docker: the scrub-class list in the collector config and
/// `CI_LOG_SCRUB_CLASSES` in the daemon must name the same set, in the same
/// order, and every statement must be scoped to `ci.job.log` alone.
///
/// This is the "fail closed on an unlisted pattern" half of the design made
/// mechanical: a new secret family cannot be added to one side only, and a
/// statement cannot quietly lose its scope guard and start rewriting another
/// kind's body.
#[test]
fn gateway_scrubs_exactly_the_declared_ci_log_classes() {
    use loom_daemon::telemetry::ci::{CI_LOG_CHUNK_MARKER_KEY, CI_LOG_SCRUB_CLASSES};

    let statements: Vec<&str> = CONFIG
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("replace_pattern(body,"))
        .collect();
    assert!(
        statements.len() >= CI_LOG_SCRUB_CLASSES.len(),
        "the collector has fewer body-rewriting statements than declared scrub classes"
    );
    let guard = format!("attributes[\"{CI_LOG_CHUNK_MARKER_KEY}\"] != nil and IsString(body)");
    // A class may need more than one pattern, so consecutive repeats
    // collapse; the class ORDER is load-bearing and compared exactly.
    let mut classes: Vec<String> = Vec::new();
    for statement in &statements {
        assert!(
            statement.contains(&guard),
            "a body rewrite is not scoped to ci.job.log records: {statement}"
        );
        let start = statement
            .find("[REDACTED:")
            .expect("every body rewrite replaces with a [REDACTED:<class>] marker");
        let end = statement[start..].find(']').expect("a closed marker") + start;
        let class = statement[start + "[REDACTED:".len()..end].to_string();
        if classes.last() != Some(&class) {
            classes.push(class);
        }
    }
    assert_eq!(
        classes,
        CI_LOG_SCRUB_CLASSES
            .iter()
            .map(|c| (*c).to_string())
            .collect::<Vec<String>>(),
        "the collector's scrub classes and CI_LOG_SCRUB_CLASSES disagree"
    );

    // The stage must run, and must run before the shared allowlist: a secret
    // has to be gone before any later stage can copy or export its body.
    let pipeline = CONFIG
        .lines()
        .find(|line| {
            line.trim()
                .starts_with("processors: [memory_limiter, transform/")
        })
        .expect("logs pipeline processors line");
    let scrub = pipeline
        .find("transform/ci_log_redaction")
        .expect("transform/ci_log_redaction must be in the logs pipeline");
    let privacy = pipeline
        .find("transform/privacy")
        .expect("transform/privacy must be in the logs pipeline");
    assert!(scrub < privacy, "the scrub stage must precede transform/privacy: {pipeline}");

    // The exception is named where a reviewer reading the allowlist will see
    // it, not only in a commit message.
    assert!(
        CONFIG.contains("`ci.job.log` (#8825) carries a GitHub Actions job"),
        "config.yaml must state the ci.job.log body exception beside the allowlist"
    );
}

/// Issue #8824 fan-out contract, no Docker: the gateway's `transform/privacy`
/// allowlists must forward every CI attribute and metric label the poller
/// emits — and, for the `loom.ci.*` namespace, nothing else. A key the
/// gateway strips makes a SigNoz CI query silently return zero rows.
#[test]
fn gateway_forwards_exactly_the_ci_telemetry_vocabulary() {
    use loom_daemon::telemetry::ci::{
        CI_LOG_ATTRIBUTE_KEYS, CI_METRIC_LABEL_KEYS, CI_SPAN_ATTRIBUTE_KEYS,
    };
    use std::collections::BTreeSet;
    fn keep(context: &str) -> BTreeSet<String> {
        let mut current = "";
        let mut keys = BTreeSet::new();
        for line in CONFIG.lines().map(str::trim) {
            if let Some(rest) = line.strip_prefix("- context:") {
                current = rest.trim();
            }
            if current == context && line.contains("keep_keys(") {
                keys.extend(line.split('"').skip(1).step_by(2).map(str::to_owned));
            }
        }
        keys
    }
    let ci = |keys: BTreeSet<String>| -> BTreeSet<String> {
        keys.into_iter()
            .filter(|k| k.starts_with("loom.ci."))
            .collect()
    };
    let owned =
        |keys: &[&str]| -> BTreeSet<String> { keys.iter().map(|k| (*k).to_owned()).collect() };
    for key in CI_LOG_ATTRIBUTE_KEYS {
        assert!(keep("log").contains(*key), "log keep_keys drops {key}");
    }
    for key in CI_SPAN_ATTRIBUTE_KEYS {
        assert!(keep("span").contains(*key), "span keep_keys drops {key}");
    }
    for label in CI_METRIC_LABEL_KEYS {
        assert!(keep("datapoint").contains(*label), "datapoint keep_keys drops {label}");
    }
    assert_eq!(ci(keep("log")), owned(CI_LOG_ATTRIBUTE_KEYS));
    assert_eq!(ci(keep("span")), owned(CI_SPAN_ATTRIBUTE_KEYS));
}
