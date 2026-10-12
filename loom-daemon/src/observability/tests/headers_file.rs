//! `headers_file` on an `observability.exporters` entry (Issue #10961): the
//! config surface, the policy pass, and what `spawn_task` reports when the
//! file is refused. The wire behavior is covered next to the exporter, in
//! `otlp/tests/headers_file.rs`; the file grammar in `request_headers/tests.rs`.
use super::*;

/// A synthetic value no status detail or log line may contain.
#[cfg(all(feature = "otlp", unix))]
const SECRET: &str = "fixture-secret-b81d44";

fn with_headers(kind: &str, endpoint: Option<&str>, headers_file: &str) -> RawExporterEntry {
    RawExporterEntry {
        headers_file: Some(headers_file.to_string()),
        ..raw(kind, endpoint)
    }
}

#[cfg(all(feature = "otlp", unix))]
fn write_headers_file(dir: &Path, contents: &str, mode: u32) -> String {
    let path = dir.join("otlp-headers");
    std::fs::write(&path, contents).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
#[serial(loom_config_env)]
fn read_config_parses_headers_file_on_an_exporter_entry() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"observability": {
            "enabled": true,
            "exporters": [
                {"kind": "otlp", "endpoint": "https://otlp.internal", "headers_file": "/run/secrets/otlp-headers"},
                {"kind": "https", "headersFile": "/run/secrets/alias"},
                {"kind": "other", "headers_file": 7},
                {"kind": "plain", "endpoint": "https://plain.internal"}
            ]
        }}"#,
    )
    .unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let config = read_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        config.exporters,
        Some(vec![
            with_headers("otlp", Some("https://otlp.internal"), "/run/secrets/otlp-headers"),
            with_headers("https", None, "/run/secrets/alias"),
            // Present but not a string: kept as an unusable path so the
            // policy pass refuses the entry rather than dropping the key.
            with_headers("other", None, ""),
            raw("plain", Some("https://plain.internal")),
        ])
    );
}

#[test]
#[serial]
fn resolve_exporters_carries_the_headers_file_path_through() {
    clear_env();
    let config = ObservabilityConfig {
        exporters: Some(vec![
            raw("https", None),
            with_headers("otlp", Some("https://otlp.internal"), "/run/secrets/otlp-headers"),
        ]),
        ..Default::default()
    };
    assert_eq!(
        resolve_exporters(&config),
        vec![
            entry(ExporterKind::Https, None),
            ExporterEntry {
                headers_file: Some("/run/secrets/otlp-headers".to_string()),
                ..entry(ExporterKind::Otlp, Some("https://otlp.internal"))
            },
        ]
    );
}

/// The policy pass, on every build: a `headers_file` entry is refused when
/// it is not `otlp`, has no usable path, or would send the headers in
/// cleartext off the host — and an entry without one is judged exactly as
/// before (plain `http` to a non-loopback collector stays allowed).
#[test]
fn entry_endpoint_applies_the_headers_file_policy_only_to_entries_that_have_one() {
    let shared = "https://ingest.test-fixture.internal".to_string();
    let refused = |entry: &ExporterEntry| entry_endpoint(entry, Some(&shared)).unwrap_err().1;

    let https_kind = ExporterEntry {
        headers_file: Some("/run/secrets/otlp-headers".to_string()),
        ..entry(ExporterKind::Https, None)
    };
    assert!(refused(&https_kind).contains("only supported on an otlp"));

    assert_eq!(
        entry_endpoint(&entry(ExporterKind::Https, None), Some(&shared)),
        Ok(shared.clone())
    );

    #[cfg(feature = "otlp")]
    {
        let otlp = |endpoint: &str, headers_file: Option<&str>| ExporterEntry {
            headers_file: headers_file.map(str::to_string),
            ..entry(ExporterKind::Otlp, Some(endpoint))
        };
        let cleartext = "http://collector.test-fixture.internal:4318";
        assert_eq!(
            entry_endpoint(&otlp(cleartext, None), Some(&shared)),
            Ok(cleartext.to_string()),
            "no headers_file ⇒ the endpoint rules are unchanged"
        );
        assert!(refused(&otlp(cleartext, Some("/run/secrets/otlp-headers"))).contains("cleartext"));
        assert!(
            refused(&otlp("https://otlp.test-fixture.internal", Some(""))).contains("non-empty")
        );
        for endpoint in [
            "https://otlp.test-fixture.internal",
            "http://127.0.0.1:4318",
        ] {
            assert_eq!(
                entry_endpoint(&otlp(endpoint, Some("/run/secrets/otlp-headers")), Some(&shared)),
                Ok(endpoint.to_string())
            );
        }
    }
}

#[cfg(feature = "otlp")]
#[test]
#[serial]
fn planned_otlp_exporters_pairs_each_endpoint_with_its_headers_file() {
    clear_env();
    let config = ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some("https://ingest.test-fixture.internal".to_string()),
        exporters: Some(vec![
            raw("https", None),
            with_headers("otlp", Some("https://otlp.test-fixture.internal"), "/run/secrets/h"),
        ]),
        ..Default::default()
    };
    assert_eq!(
        planned_otlp_exporters(&config),
        vec![(
            "https://otlp.test-fixture.internal".to_string(),
            Some("/run/secrets/h".to_string())
        )]
    );
    assert_eq!(planned_otlp_endpoints(&config), vec!["https://otlp.test-fixture.internal"]);
}

/// A `headers_file` on a non-otlp entry takes that sink out with its own
/// `misconfigured` status, before any file is read. Runs on every build.
#[tokio::test]
#[serial]
async fn spawn_task_refuses_a_headers_file_on_a_non_otlp_entry() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let config = ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        // Never read: the entry is refused in the policy pass.
        ingest_key_file: Some(dir.path().join("absent").to_string_lossy().into_owned()),
        exporters: Some(vec![with_headers("https", None, "/nonexistent/otlp-headers")]),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
    assert!(handles.is_none());
    let statuses = global_export_statuses();
    assert_eq!(statuses["https"].state, crate::types::ObservabilityExportState::Misconfigured);
    assert!(statuses["https"]
        .last_failure_detail
        .as_deref()
        .unwrap()
        .contains("only supported on an otlp"));
    assert_eq!(bus.receiver_count(), 0);
}

/// A refused headers file — loose permissions, then a malformed line — turns
/// the otlp sink `misconfigured` with a detail naming the file (and line),
/// and neither the status nor any log line carries a header value.
#[cfg(all(feature = "otlp", unix))]
#[tokio::test]
#[serial]
async fn spawn_task_reports_a_refused_headers_file_without_its_contents() {
    clear_env();
    for (contents, mode, expected) in [
        (format!("X-Client-Secret: {SECRET}\n"), 0o644, "group or others".to_string()),
        (format!("X-Client-Id: fixture\n{SECRET}\n"), 0o600, "line 2:".to_string()),
    ] {
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("ingest.key");
        std::fs::write(&key_path, "fixture-ingest-key\n").unwrap();
        let headers_path = write_headers_file(dir.path(), &contents, mode);
        let config = ObservabilityConfig {
            enabled: Some(true),
            ingest_key_file: Some(key_path.to_string_lossy().to_string()),
            exporters: Some(vec![with_headers(
                "otlp",
                Some("https://otlp.test-fixture.internal"),
                &headers_path,
            )]),
            flush_interval_secs: Some(3600),
            ..Default::default()
        };
        let pool = test_workspace_pool();
        let mut handles = None;
        // `#[tokio::test]` is current-thread, so `spawn_task`'s own log
        // lines land in this thread's capture buffer.
        let logs = crate::test_log_capture::capture_logs(|| {
            handles = spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), pool);
        });
        assert!(handles.is_none(), "the only exporter was refused");
        let status = &global_export_statuses()["otlp"];
        assert_eq!(status.state, crate::types::ObservabilityExportState::Misconfigured);
        let detail = status.last_failure_detail.clone().unwrap();
        assert!(detail.contains(&headers_path), "{detail}");
        assert!(detail.contains(&expected), "{detail}");
        let status_json = serde_json::to_string(status).unwrap();
        assert!(!status_json.contains(SECRET), "{status_json}");
        assert!(
            logs.iter()
                .any(|(_, message)| message.contains(&headers_path)),
            "the refusal is logged by path: {logs:?}"
        );
        for (_, message) in &logs {
            assert!(!message.contains(SECRET), "log line leaked a header value: {message}");
        }
    }
}

/// The happy path through `spawn_task`: an owner-only file on an otlp entry
/// starts the sink like any other.
#[cfg(all(feature = "otlp", unix))]
#[tokio::test]
#[serial]
async fn spawn_task_starts_an_otlp_exporter_with_a_headers_file() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let key_path = dir.path().join("ingest.key");
    std::fs::write(&key_path, "fixture-ingest-key\n").unwrap();
    let headers_path =
        write_headers_file(dir.path(), &format!("X-Client-Secret: {SECRET}\n"), 0o600);
    let config = ObservabilityConfig {
        enabled: Some(true),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        exporters: Some(vec![with_headers(
            "otlp",
            Some("https://otlp.test-fixture.internal"),
            &headers_path,
        )]),
        flush_interval_secs: Some(3600), // no network attempt during the test
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool())
            .expect("a valid headers file must not stop the exporter");
    let status = &global_export_statuses()["otlp"];
    assert_ne!(status.state, crate::types::ObservabilityExportState::Misconfigured);
    assert!(!serde_json::to_string(status).unwrap().contains(SECRET));
    for handle in handles {
        handle.abort();
    }
}
