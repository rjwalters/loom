//! Live proof for the cycle-time analytics artifacts (Issue #8665): requires
//! Docker, never converts missing Docker to a pass. The static half is
//! `cycle_time_artifacts.rs`, which runs everywhere.
//!
//! What this establishes that no static check can. The committed queries are
//! executed **verbatim**, against rows written by the real OpenTelemetry
//! ClickHouse exporter from real `loom-daemon telemetry-export` output, in the
//! pinned ClickHouse the SigNoz trial uses. Three of the design's assumptions
//! are observations here rather than beliefs:
//!
//! 1. **How a nested attribute lands.** `loom.phase_durations` is an OTLP array
//!    of key-value lists, and the ClickHouse log schema stores attributes in a
//!    `Map(String, String)`. The exporter serializes it to JSON with its keys in
//!    ALPHABETICAL order (`duration_sec` before `phase`), so a positional tuple
//!    extraction would silently read durations as phase names. The artifacts
//!    extract to a *named* tuple; this test is what proves that was necessary.
//! 2. **Which column carries the record kind.** The created schema has no
//!    event-name column at all, so the filter has to be on `Body`.
//! 3. **That the rollup outlives the raw signal.** The raw table is created with
//!    the ClickStack deployment's real 168h TTL — asserted below — and the last
//!    phase deletes every raw row and re-asks the headline question, which must
//!    still answer identically. Expiry is simulated by an explicit delete rather
//!    than by waiting on a background merge: ClickHouse TTL is not a precise
//!    deadline, and a test must not race one.
//!
//! The fixture's first draft used fixed calendar timestamps and lost six of its
//! seven rows on insert to that same 168h TTL. That is the data loss this whole
//! artifact set exists to survive, so the fixture is relative-time now and the
//! incident is recorded in the template's own header.
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};

/// Same pin as the SigNoz trial's telemetry store (`signoz/casting.yaml`), so
/// this proof and that deployment can never drift apart on ClickHouse version.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";
/// Same pin as the OTLP transport proof (`otlp_collector.rs`).
const COLLECTOR_IMAGE: &str = "otel/opentelemetry-collector-contrib:0.139.0@sha256:faf125d656fa47cea568b2f3b4494efd2525083bc75c1e96038bc23f05cd68fd";

/// Containers and the network they share, removed even on panic.
struct Stack {
    network: String,
    clickhouse: String,
    collector: String,
    password: String,
}

impl Drop for Stack {
    fn drop(&mut self) {
        for name in [&self.collector, &self.clickhouse] {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
    }
}

fn run(command: &mut Command) -> String {
    let output = command.output().expect("docker is required for this test");
    assert!(
        output.status.success(),
        "command failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn observability_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../defaults/observability")
        .canonicalize()
        .unwrap()
}

impl Stack {
    fn start() -> Self {
        let tag = format!(
            "loom-8665-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let stack = Stack {
            network: format!("{tag}-net"),
            clickhouse: format!("{tag}-clickhouse"),
            collector: format!("{tag}-collector"),
            // Ephemeral, per-run, local-only: the pinned image refuses an empty
            // password for `default` from a non-local client, and a committed
            // constant would be a credential in the repository.
            password: format!("synthetic-{tag}"),
        };
        run(Command::new("docker").args(["network", "create", &stack.network]));
        run(Command::new("docker").args([
            "run",
            "--detach",
            "--name",
            &stack.clickhouse,
            "--network",
            &stack.network,
            "--network-alias",
            "clickhouse",
            "--env",
            &format!("CLICKHOUSE_PASSWORD={}", stack.password),
            "--ulimit",
            "nofile=262144:262144",
            CLICKHOUSE_IMAGE,
        ]));
        let deadline = Instant::now() + Duration::from_secs(120);
        while stack.try_query("SELECT 1").is_none() {
            assert!(Instant::now() < deadline, "ClickHouse never became ready");
            std::thread::sleep(Duration::from_millis(500));
        }
        run(Command::new("docker")
            .args([
                "run",
                "--detach",
                "--name",
                &stack.collector,
                "--network",
                &stack.network,
                "--publish",
                "127.0.0.1::4318",
                "--env",
                &format!("CLICKHOUSE_PASSWORD={}", stack.password),
                "--volume",
            ])
            .arg(format!(
                "{}:/etc/otelcol-contrib/config.yaml:ro",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/cycle_time/collector.yaml")
                    .display()
            ))
            .arg(COLLECTOR_IMAGE));
        stack
    }

    /// The collector exits non-zero when its exporter cannot reach ClickHouse,
    /// so a dead container is reported with its logs instead of as a timeout.
    fn wait_for_collector(&self) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let state = run(Command::new("docker").args([
                "inspect",
                "-f",
                "{{.State.Status}}",
                &self.collector,
            ]));
            assert!(
                state.trim() == "running",
                "collector is {} — logs:\n{}",
                state.trim(),
                run(Command::new("docker").args(["logs", &self.collector]))
            );
            let port = Command::new("docker")
                .args(["port", &self.collector, "4318/tcp"])
                .output()
                .unwrap();
            if port.status.success() {
                let address = String::from_utf8_lossy(&port.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                if !address.is_empty() && std::net::TcpStream::connect(&address).is_ok() {
                    return address;
                }
            }
            assert!(Instant::now() < deadline, "collector never published 4318");
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn client(&self) -> Command {
        let mut command = Command::new("docker");
        command.args([
            "exec",
            "--interactive",
            "--env",
            &format!("CLICKHOUSE_PASSWORD={}", self.password),
            &self.clickhouse,
            "clickhouse-client",
        ]);
        command
    }

    fn try_query(&self, sql: &str) -> Option<String> {
        let output = self
            .client()
            .args(["--query", sql])
            .output()
            .expect("docker is required for this test");
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// One statement, returned as tab-separated rows.
    fn query(&self, sql: &str, params: &[(&str, &str)]) -> Vec<Vec<String>> {
        let mut command = self.client();
        for (key, value) in params {
            command.arg(format!("--param_{key}={value}"));
        }
        let text = run(command.args(["--query", sql]));
        text.lines()
            .map(|line| line.split('\t').map(str::to_owned).collect())
            .collect()
    }

    fn scalar(&self, sql: &str) -> String {
        self.query(sql, &[])
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or_default()
    }

    /// A committed artifact, executed verbatim from the repository.
    fn run_script(&self, sql: &str, params: &[(&str, &str)]) {
        let mut command = self.client();
        command.arg("--multiquery");
        for (key, value) in params {
            command.arg(format!("--param_{key}={value}"));
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("docker is required for this test");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(sql.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "committed SQL artifact failed to execute: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The committed text of one canonical question, cut from the artifact at its
/// `-- CTn.` marker so the test can never drift from what ships.
fn canonical_question(queries: &str, id: &str) -> String {
    let marker = format!("-- {id}. ");
    let start = queries
        .find(&marker)
        .unwrap_or_else(|| panic!("cycle-time-queries.sql has no {id}"));
    let body = &queries[start..];
    let end = body.find(";\n").expect("question is not terminated");
    body[..=end].to_owned()
}

/// Renders the relative-time fixture: `#` lines dropped, `{{hours_ago:N}}`
/// replaced with an instant N hours before `now`.
fn render_fixture(template: &str, now: DateTime<Utc>) -> String {
    let mut rendered = String::new();
    for line in template.lines().filter(|line| !line.starts_with('#')) {
        let mut remaining = line;
        while let Some(open) = remaining.find("{{hours_ago:") {
            let close = remaining[open..].find("}}").unwrap() + open;
            let hours: i64 = remaining[open + "{{hours_ago:".len()..close]
                .parse()
                .unwrap();
            rendered.push_str(&remaining[..open]);
            rendered.push_str(
                &(now - ChronoDuration::hours(hours)).to_rfc3339_opts(SecondsFormat::Secs, true),
            );
            remaining = &remaining[close + 2..];
        }
        rendered.push_str(remaining);
        rendered.push('\n');
    }
    rendered
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_cycle_time_artifacts_answer_the_canonical_questions_on_real_exported_rows() {
    let observability = observability_dir();
    let extract =
        std::fs::read_to_string(observability.join("clickstack/cycle-time-extract.sql")).unwrap();
    let rollup = std::fs::read_to_string(observability.join("cycle-time-rollup.sql")).unwrap();
    let queries = std::fs::read_to_string(observability.join("cycle-time-queries.sql")).unwrap();

    let stack = Stack::start();
    let address = stack.wait_for_collector();

    // ---- export the fixture through the real OTLP path ------------------
    let now = Utc::now();
    let workdir = tempfile::tempdir().unwrap();
    let envelopes = workdir.path().join("envelopes.jsonl");
    let template = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cycle_time/envelopes.jsonl.tmpl"),
    )
    .unwrap();
    std::fs::write(&envelopes, render_fixture(&template, now)).unwrap();
    let key = workdir.path().join("key");
    std::fs::write(&key, "synthetic-test-key\n").unwrap();
    run(Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("telemetry-export")
        .arg("--input")
        .arg(&envelopes)
        .arg("--endpoint")
        .arg(format!("http://{address}"))
        .arg("--key-file")
        .arg(&key));

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let rows =
            stack.scalar("SELECT count() FROM default.otel_logs WHERE Body = 'sweep.outcome'");
        if rows.trim() == "7" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the exporter never wrote all 7 fixture outcome rows (saw {rows})"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // ---- the three assumptions the artifacts are built on ---------------
    let created = stack.scalar(
        "SELECT create_table_query FROM system.tables WHERE database = 'default' AND name = 'otel_logs'",
    );
    assert!(
        created.contains("TTL TimestampTime + toIntervalDay(7)"),
        "the raw log table was not created with the 168h retention the artifacts reason about: \
         {created}"
    );
    assert!(
        !created.contains("EventName"),
        "the log schema now has an event-name column; the artifacts filter on Body because it \
         did not: {created}"
    );
    let attributes = stack.scalar(
        "SELECT LogAttributes['loom.phase_durations'] FROM default.otel_logs \
         WHERE LogAttributes['loom.sweep_id'] = 'ship-alpha-106'",
    );
    assert_eq!(
        attributes.trim(),
        r#"[{"duration_sec":420,"phase":"builder"},{"duration_sec":60,"phase":"judge"}]"#,
        "the exporter's serialization of the nested phase-duration attribute changed; the \
         extraction reads it as a NAMED tuple precisely because the keys are not in declaration \
         order"
    );
    // Every key the extraction reads must actually be emitted, not merely
    // allowed through the gateway. Read off the real rows, not from a list.
    for key in [
        "loom.repo",
        "loom.repo.visibility",
        "loom.sweep_id",
        "loom.issue",
        "loom.result",
        "loom.total_duration_sec",
        "loom.phase_durations",
        "loom.pr_number",
        "loom.doctor_cycles",
        "loom.failure_class",
        "loom.runtime",
        "loom.provider",
        "loom.model",
        "loom.configured_model",
        "loom.effort",
    ] {
        let present = stack.scalar(&format!(
            "SELECT countIf(mapContains(LogAttributes, '{key}')) FROM default.otel_logs"
        ));
        assert_ne!(
            present.trim(),
            "0",
            "no exported sweep.outcome row carries '{key}', which the extraction view reads"
        );
    }

    // ---- build the rollup from the committed artifacts -------------------
    let since = (now - ChronoDuration::days(8))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let until = (now + ChronoDuration::days(1))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let window: &[(&str, &str)] = &[("since", &since), ("until", &until)];
    stack.run_script(&extract, &[]);
    stack.run_script(&rollup, window);

    // At-least-once delivery: seven raw rows, six ships.
    assert_eq!(
        stack
            .scalar("SELECT count() FROM loom_analytics.ship")
            .trim(),
        "6",
        "the replayed duplicate delivery was counted as a second ship"
    );

    // ---- CT1: the acceptance question ------------------------------------
    let ct1 = stack.query(
        &canonical_question(&queries, "CT1"),
        &[("since", &since), ("until", &until), ("top_n", "10")],
    );
    let headline: Vec<(String, String, String)> = ct1
        .iter()
        .map(|row| (row[3].clone(), row[6].clone(), row[7].clone()))
        .collect();
    assert_eq!(
        headline,
        vec![
            // The slowest ship of the window carries no phase breakdown at all:
            // `\N` is NULL, and must never be read as "no slow phase".
            ("ship-beta-103".into(), "5400".into(), "\\N".into()),
            ("ship-alpha-101".into(), "3600".into(), "builder".into()),
            // 800s + 700s of `judge` across a repair loop outranks a single
            // 900s `builder`; summing per phase name is what makes that true.
            ("ship-alpha-102".into(), "2960".into(), "judge".into()),
            ("ship-alpha-106".into(), "480".into(), "builder".into()),
            ("ship-alpha-105".into(), "265".into(), "builder".into()),
        ],
        "CT1 no longer answers 'the slowest ships, and which phase dominated each'"
    );
    assert!(
        !ct1.iter().any(|row| row[3] == "ship-beta-104"),
        "CT1 included a failed sweep among the ships"
    );

    // ---- the remaining canonical questions --------------------------------
    let ct2 = stack.query(&canonical_question(&queries, "CT2"), window);
    assert_eq!(ct2[0][0], "builder", "CT2 no longer ranks phases by total time");
    assert_eq!(
        ct2.iter()
            .find(|row| row[0] == "judge")
            .map(|row| row[7].clone()),
        Some("1910".to_owned()),
        "CT2's per-phase totals changed"
    );

    let ct3 = stack.query(&canonical_question(&queries, "CT3"), window);
    assert_eq!(
        ct3.iter()
            .find(|row| row[0] == "synthetic/beta")
            .map(|row| row[3].clone()),
        Some("50".to_owned()),
        "CT3 no longer reports the per-repo success rate"
    );

    let ct4 = stack.query(&canonical_question(&queries, "CT4"), window);
    assert!(
        ct4.iter().any(|row| row[0] == "\\N"),
        "CT4 collapsed an unreported runtime into a bucket instead of leaving it NULL"
    );

    let ct5 = stack.query(&canonical_question(&queries, "CT5"), window);
    let repaired = ct5
        .iter()
        .find(|row| row[0] == "1")
        .expect("CT5 no longer separates repaired ships");
    assert_eq!(repaired[1], "1", "CT5 counted the wrong number of repaired ships");
    assert_eq!(repaired[6], "400", "CT5 lost the doctor-phase seconds");

    let ct6 = stack.query(&canonical_question(&queries, "CT6"), window);
    let weeks: u32 = ct6.iter().map(|row| row[1].parse::<u32>().unwrap()).sum();
    assert_eq!(weeks, 6, "CT6 lost ships while bucketing them by week");

    let ct7 = stack.query(&canonical_question(&queries, "CT7"), window);
    assert_eq!(
        ct7[0],
        vec!["6", "1", "1", "1", "1", "2", "1", "2", "0"],
        "CT7's coverage counts changed; an absent measurement may have become a zero"
    );

    let ct8 = stack.query(&canonical_question(&queries, "CT8"), window);
    assert_eq!(
        ct8[0],
        vec!["6", "6", "0", "0", "0"],
        "CT8 reports drift between the raw logs and the rollup"
    );

    // ---- the retention claim, exercised ----------------------------------
    // Simulate the raw rows reaching the 168h TTL. The rollup must keep
    // answering the acceptance question identically, and CT8 must classify the
    // difference as "beyond raw retention" rather than as drift.
    stack.run_script("TRUNCATE TABLE default.otel_logs;", &[]);
    assert_eq!(stack.scalar("SELECT count() FROM default.otel_logs").trim(), "0");
    let after_expiry = stack.query(
        &canonical_question(&queries, "CT1"),
        &[("since", &since), ("until", &until), ("top_n", "10")],
    );
    assert_eq!(
        after_expiry, ct1,
        "the headline question stopped answering once the raw rows expired — the rollup did not \
         outlive the raw retention window, which is the entire point of it"
    );
    let ct8_after = stack.query(&canonical_question(&queries, "CT8"), window);
    assert_eq!(
        ct8_after[0],
        vec!["0", "6", "0", "6", "0"],
        "after expiry CT8 must report six ships held beyond raw retention and zero drift"
    );

    // A backfill re-run over the now-empty raw window must not damage history.
    stack.run_script(&rollup, window);
    assert_eq!(
        stack
            .scalar("SELECT count() FROM loom_analytics.ship")
            .trim(),
        "6",
        "re-running the backfill after expiry lost rolled-up history"
    );
}
