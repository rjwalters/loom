//! Live proof for `signoz/github-shadow.sql` (#10343): the committed GitHub
//! shadow-spend queries, executed verbatim by `clickhouse local` in the same
//! pinned image the SigNoz trial's telemetry store runs, over the rows in
//! `fixtures/signoz_github_shadow/fixture.sql`. Requires Docker; never
//! converts missing Docker to a pass.
//!
//! The PR #10565 review found that the first version of queries 1 and 3
//! treated any drop in a bucket's per-minute `used` as a quota reset. A host
//! that stops reporting while another keeps exporting an older reading of the
//! same bucket, or two buckets interleaved under one label set
//! (rjwalters/loom#10571), then re-charged the whole lower reading every time.
//! On live data that billed one installation's core bucket 61,653 requests in
//! under two hours. The queries now key every reading by its quota window (the
//! paired `github.ratelimit.reset`) and charge each window's high-water mark
//! once. This test pins that:
//!
//! - **stale surviving readings** inside one window change nothing;
//! - a **genuine reset** (new reset epoch) starts a new charge;
//! - **reset jitter** of a second is still one window;
//! - **two interleaved windows** under one label set are each counted once;
//! - an **owner-less** pre-#10343 point is ignored;
//! - query 3's **band** and query 4's span counts come out as documented;
//! - the **agent slice** (#10607): agent rows count as attributed, so the
//!   band shrinks by `agent_share`, and query 5 splits them by role and
//!   served/passthrough.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Same pin as `signoz_queue_quota_queries.rs` and the trial's telemetry store.
const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";

const SHADOW: &str = include_str!("../../defaults/observability/signoz/github-shadow.sql");
const FIXTURE: &str = include_str!("fixtures/signoz_github_shadow/fixture.sql");

type Row = BTreeMap<String, serde_json::Value>;

fn clickhouse(script: &str, format: &str) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "clickhouse",
            CLICKHOUSE_IMAGE,
            "local",
            "--multiquery",
            &format!("--format={format}"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "clickhouse rejected the script:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The committed statements, line comments stripped before the split on `;`
/// (no `--` or `;` occurs inside a string literal in the file).
fn statements(sql: &str) -> Vec<String> {
    let code = sql
        .lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n");
    code.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Every committed query's rows, in file order.
fn sections() -> Vec<Vec<Row>> {
    let committed = statements(SHADOW);
    let mut script = String::from(FIXTURE);
    for (index, statement) in committed.iter().enumerate() {
        script.push_str(&format!("\nSELECT {index} AS loom_section_marker;\n{statement};\n"));
    }
    let mut sections: Vec<Vec<Row>> = Vec::new();
    for line in clickhouse(&script, "JSONEachRow").lines() {
        let row: Row = serde_json::from_str(line).expect("JSONEachRow line");
        if row.contains_key("loom_section_marker") {
            sections.push(Vec::new());
        } else {
            sections.last_mut().expect("marker first").push(row);
        }
    }
    assert_eq!(sections.len(), committed.len(), "every query produced a section");
    sections
}

fn num(row: &Row, key: &str) -> f64 {
    let v = row
        .get(key)
        .unwrap_or_else(|| panic!("column {key} missing from {row:?}"));
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("column {key} is not a number in {row:?}"))
}

fn text<'a>(row: &'a Row, key: &str) -> &'a str {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("column {key} is not a string in {row:?}"))
}

/// `(account, owner, resource)` -> per-hour values of `column`, hour order.
fn by_bucket(rows: &[Row], account: &str, column: &str) -> Vec<f64> {
    rows.iter()
        .filter(|r| r.get("account").and_then(|v| v.as_str()) == Some(account))
        .map(|r| num(r, column))
        .collect()
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn committed_github_shadow_queries_charge_each_quota_window_once() {
    // The file runs verbatim, comments included, as the README says.
    clickhouse(&format!("{FIXTURE}\n{SHADOW}"), "TSV");

    let s = sections();
    assert_eq!(s.len(), 6, "queries 0-5");

    // 0. preflight names all three families; loom.forge.calls is Delta.
    let families: BTreeMap<&str, &str> = s[0]
        .iter()
        .map(|r| (text(r, "metric_name"), text(r, "temporality")))
        .collect();
    assert_eq!(families.get("loom.forge.calls"), Some(&"Delta"));
    assert!(families.contains_key("github.ratelimit.used"));
    assert!(families.contains_key("github.ratelimit.reset"));
    // #10571: the installation witness label is read (absent / `-` excluded).
    let installations: BTreeMap<&str, f64> = s[0]
        .iter()
        .map(|r| (text(r, "metric_name"), num(r, "installations")))
        .collect();
    assert_eq!(installations.get("github.ratelimit.used"), Some(&2.0), "{installations:?}");
    assert_eq!(installations.get("loom.forge.calls"), Some(&1.0), "{installations:?}");

    // 1. Bucket A: stale readings ignored, genuine reset charged, jitter is
    //    one window -> 190 in hour H, 40 in hour H+1 (the pre-fix recipe
    //    billed 240 in hour H). Bucket B: two interleaved windows -> 4195
    //    (the pre-fix recipe billed 8250).
    let q1 = &s[1];
    assert!(
        q1.iter().all(|r| !text(r, "owner").is_empty()),
        "owner-less points are not buckets: {q1:?}"
    );
    assert_eq!(by_bucket(q1, "app-1", "github_used"), [190.0, 40.0], "{q1:?}");
    assert_eq!(by_bucket(q1, "app-1", "windows"), [2.0, 1.0], "{q1:?}");
    assert_eq!(by_bucket(q1, "app-2", "github_used"), [4195.0], "{q1:?}");
    assert_eq!(by_bucket(q1, "app-2", "windows"), [2.0], "{q1:?}");
    assert!(
        q1.iter().all(|r| num(r, "github_used") < 9999.0),
        "the legacy owner-less 9999 reading leaked in: {q1:?}"
    );

    // 2. attributed: daemon ok 150 + agent ok 20, ok+error 190, 304s 50 + 25;
    //    the free probe excluded; the agent slice is its own column.
    let q2 = &s[2];
    assert_eq!(q2.len(), 1, "{q2:?}");
    assert_eq!(text(&q2[0], "resource"), "core");
    assert_eq!(num(&q2[0], "attributed_min"), 170.0);
    assert_eq!(num(&q2[0], "attributed_max"), 190.0);
    assert_eq!(num(&q2[0], "free_304"), 75.0);
    assert_eq!(num(&q2[0], "agent_attributed"), 20.0);
    assert_eq!(num(&q2[0], "agent_free_304"), 25.0);

    // 3. the band for bucket A, hour H; NULL-safe for the hour with no calls.
    //    Without the agent rows it was 0.105 .. 0.211: the agent slice
    //    shrinks it by agent_share.
    let q3: Vec<&Row> = s[3]
        .iter()
        .filter(|r| text(r, "account") == "app-1")
        .collect();
    assert_eq!(num(q3[0], "github_used"), 190.0, "{q3:?}");
    assert!(num(q3[0], "shadow_low").abs() < 1e-9, "{q3:?}");
    assert!((num(q3[0], "shadow_high") - 0.105).abs() < 1e-9, "{q3:?}");
    assert!((num(q3[0], "agent_share") - 0.105).abs() < 1e-9, "{q3:?}");
    assert_eq!(num(q3[1], "attributed_min"), 0.0, "{q3:?}");
    assert_eq!(num(q3[1], "agent_attributed"), 0.0, "{q3:?}");

    // 4. span cross-check: two `ok` spans, 3 known requests, 1 unknown.
    let q4 = &s[4];
    assert_eq!(q4.len(), 1, "{q4:?}");
    assert_eq!(num(&q4[0], "spans"), 2.0);
    assert_eq!(num(&q4[0], "known_requests"), 3.0);
    assert_eq!(num(&q4[0], "unknown_request_spans"), 1.0);
    assert_eq!(num(&q4[0], "unknown_status_spans"), 1.0);

    // 5. the agent slice by role: daemon rows (`-` or no label) excluded.
    let q5: Vec<(&str, &str, f64, f64)> = s[5]
        .iter()
        .map(|r| (text(r, "agent"), text(r, "via"), num(r, "charged"), num(r, "free_304")))
        .collect();
    assert_eq!(
        q5,
        [
            ("builder", "passthrough", 15.0, 0.0),
            ("judge", "served", 5.0, 25.0)
        ],
        "{:?}",
        s[5]
    );
}
