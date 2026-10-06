//! Golden-output fidelity for the front's `gh pr checks` (#10516).
//!
//! `fixtures/pr_checks/` holds, per PR of rjwalters/loom, the REST payloads
//! the front reads (`<pr>.pull|runs|status.json`, trimmed to the fields any
//! reader uses) and what the real `gh` printed for the same head commit
//! (`<pr>-<shape>.stdout|stderr|exit`; version in `GH_VERSION`). The REST
//! reads were taken immediately before the `gh` runs and re-read after them
//! unchanged. Every served shape must match `gh` byte for byte, exit code
//! included; every shape the front cannot prove must decline — and for those
//! the real output must still be one of the outputs the front considered,
//! which pins the row model and Go's sort port against real data.

#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};

use super::*;

struct Case {
    pull: &'static str,
    runs: &'static str,
    status: &'static str,
}

macro_rules! case {
    ($pr:literal) => {
        Case {
            pull: include_str!(concat!("fixtures/pr_checks/", $pr, ".pull.json")),
            runs: include_str!(concat!("fixtures/pr_checks/", $pr, ".runs.json")),
            status: include_str!(concat!("fixtures/pr_checks/", $pr, ".status.json")),
        }
    };
}

macro_rules! golden {
    ($shape:literal) => {
        Served {
            stdout: include_str!(concat!("fixtures/pr_checks/", $shape, ".stdout")).to_string(),
            stderr: include_str!(concat!("fixtures/pr_checks/", $shape, ".stderr")).to_string(),
            code: include_str!(concat!("fixtures/pr_checks/", $shape, ".exit"))
                .trim()
                .parse()
                .unwrap(),
        }
    };
}

fn v(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

impl Case {
    fn render(&self, json: Option<&str>) -> Option<Served> {
        let fields = json.map(|j| parse_fields(j).unwrap());
        let pull = v(self.pull);
        render(
            fields.as_deref(),
            pull["head"]["ref"].as_str().unwrap(),
            &v(self.runs),
            &v(self.status),
        )
    }

    fn rows(&self) -> Vec<Row> {
        rows(&v(self.runs), &v(self.status)).unwrap()
    }

    /// Every text output some GraphQL order could produce.
    fn text_candidates(&self) -> Vec<String> {
        let rows = self.rows();
        text_outputs(&rows, &tie_groups(&rows), usize::MAX).unwrap()
    }

    /// Every `--json` output some GraphQL order could produce.
    fn json_candidates(&self, json: &str) -> Vec<String> {
        let rows = self.rows();
        let fields = parse_fields(json).unwrap();
        let mut out = Vec::new();
        for_each_order(rows.len(), &tie_groups(&rows), &mut |order| {
            let objs: Vec<String> =
                order.iter().map(|&i| rows[i].json_object(&fields).unwrap()).collect();
            let s = format!("[{}]\n", objs.join(","));
            if !out.contains(&s) {
                out.push(s);
            }
            true
        })
        .unwrap();
        out
    }
}

#[test]
fn text_with_failures_is_served_byte_for_byte() {
    // Fail + pass + skipping, three tied pairs: all 8 orders print the same.
    let got = case!("10543").render(None).unwrap();
    assert_eq!(got, golden!("10543-text"));
    assert_eq!(got.code, 1);
}

#[test]
fn json_whose_ties_project_identically_is_served_byte_for_byte() {
    let got = case!("10551")
        .render(Some("bucket,state,startedAt"))
        .unwrap();
    assert_eq!(got, golden!("10551-json-bucket-state-startedAt"));
    // `--json` exits 0 whatever the buckets.
    assert_eq!(got.code, 0);
}

#[test]
fn zero_checks_reproduce_the_6211_signature_in_both_modes() {
    let c = case!("10534");
    assert_eq!(c.render(None).unwrap(), golden!("10534-text"));
    assert_eq!(c.render(Some("bucket,name")).unwrap(), golden!("10534-json-bucket-name"));
    assert_eq!(
        golden!("10534-text").stderr,
        "no checks reported on the 'feature/issue-10508' branch\n"
    );
}

#[test]
fn ambiguous_text_declines_and_gh_printed_one_of_the_candidates() {
    let c = case!("10551");
    assert_eq!(c.render(None), None);
    let candidates = c.text_candidates();
    assert_eq!(candidates.len(), 2, "two tie orders print differently");
    assert!(candidates.contains(&golden!("10551-text").stdout));
    // 10543's candidates collapse to exactly gh's output.
    assert_eq!(case!("10543").text_candidates(), vec![golden!("10543-text").stdout]);
}

#[test]
fn ambiguous_json_declines_and_gh_printed_one_of_the_candidates() {
    let all = "name,state,bucket,link,startedAt,completedAt,description";
    for (c, json, shape) in [
        (case!("10543"), all, golden!("10543-json-all")),
        (case!("10551"), "bucket,name", golden!("10551-json-bucket-name")),
    ] {
        assert_eq!(c.render(Some(json)), None, "{json}");
        let candidates = c.json_candidates(json);
        assert!(candidates.len() > 1);
        assert!(candidates.contains(&shape.stdout), "{json}");
        assert_eq!((shape.stderr.as_str(), shape.code), ("", 0));
    }
}

#[test]
fn too_many_tie_orders_decline() {
    // Mid-run: 13 jobs started in the same second.
    let c = case!("10532");
    assert_eq!(c.render(None), None);
    let rows = c.rows();
    assert_eq!(for_each_order(rows.len(), &tie_groups(&rows), &mut |_| true), None);
}

fn run(name: &str, status: &str, conclusion: Option<&str>, t: (&str, Option<&str>)) -> Value {
    json!({"name": name, "status": status, "conclusion": conclusion,
           "started_at": t.0, "completed_at": t.1,
           "details_url": format!("https://ci/{name}")})
}

fn runs(rows: Vec<Value>) -> Value {
    json!({"total_count": rows.len(), "check_runs": rows})
}

const NO_STATUS: &str = r#"{"statuses": []}"#;

#[test]
fn exit_codes_and_text_buckets() {
    let t1 = ("2026-10-01T00:00:00Z", Some("2026-10-01T00:01:23Z"));
    let t2 = ("2026-10-01T00:00:05Z", Some("2026-10-01T00:00:05Z"));
    let t3 = ("2026-10-01T00:00:09Z", None);
    let ns = v(NO_STATUS);
    let out = |rows| render(None, "b", &runs(rows), &ns).unwrap();
    let fail = out(vec![
        run("a", "completed", Some("success"), t1),
        run("b", "completed", Some("timed_out"), t2),
    ]);
    assert_eq!(
        (fail.stdout.as_str(), fail.code),
        ("b\tfail\t0\thttps://ci/b\t\na\tpass\t1m23s\thttps://ci/a\t\n", 1)
    );
    let pending = out(vec![run("a", "in_progress", None, t3)]);
    assert_eq!((pending.stdout.as_str(), pending.code), ("a\tpending\t0\thttps://ci/a\t\n", 8));
    // `cancel` prints as `fail` but neither fails nor pends the exit code.
    let cancel = out(vec![run("c", "completed", Some("cancelled"), t1)]);
    assert_eq!((cancel.stdout.as_str(), cancel.code), ("c\tfail\t1m23s\thttps://ci/c\t\n", 0));
    // `startup_failure` / `stale` land in gh's default (pending) bucket.
    assert_eq!(out(vec![run("s", "completed", Some("stale"), t1)]).code, 8);
}

#[test]
fn legacy_statuses_render_as_gh_renders_status_contexts() {
    let status = json!({"statuses": [{"context": "ci/x", "state": "error",
        "target_url": null, "description": "boom \"quoted\""}]});
    let text = render(None, "b", &runs(vec![]), &status).unwrap();
    assert_eq!((text.stdout.as_str(), text.code), ("ci/x\tfail\t0\t\tboom \"quoted\"\n", 1));
    let fields = parse_fields("completedAt,description,startedAt,state").unwrap();
    let json = render(Some(fields.as_slice()), "b", &runs(vec![]), &status).unwrap();
    assert_eq!(
        json.stdout,
        "[{\"completedAt\":\"0001-01-01T00:00:00Z\",\"description\":\"boom \\\"quoted\\\"\",\
         \"startedAt\":\"0001-01-01T00:00:00Z\",\"state\":\"ERROR\"}]\n"
    );
}

#[test]
fn shapes_outside_the_contract_decline() {
    let t = ("2026-10-01T00:00:00Z", Some("2026-10-01T00:00:01Z"));
    let ns = v(NO_STATUS);
    let declines = |runs: Value, json: Option<&str>| {
        let fields = json.map(|j| parse_fields(j).unwrap());
        render(fields.as_deref(), "b", &runs, &ns).is_none()
    };
    // gh dedups by name/workflow/event; REST cannot tell those apart.
    assert!(declines(
        runs(vec![run("a", "completed", Some("success"), t), run("a", "queued", None, t)]),
        None
    ));
    assert!(declines(
        runs(vec![run("a", "completed", Some("success"), ("2026-10-01T00:00:00.5Z", None))]),
        None
    ));
    assert!(declines(
        runs(vec![run("a", "completed", Some("success"), ("2026-10-01T00:00:00+00:00", None))]),
        None
    ));
    assert!(declines(runs(vec![run("a", "completed", None, t)]), None));
    assert!(declines(runs(vec![run("a\u{1}", "completed", Some("success"), t)]), Some("name")));
    assert!(declines(json!({"check_runs": "x"}), None));
}

#[test]
fn go_duration_strings() {
    for (secs, want) in [
        (1, "1s"),
        (59, "59s"),
        (60, "1m0s"),
        (83, "1m23s"),
        (3600, "1h0m0s"),
        (3661, "1h1m1s"),
        (90_000, "25h0m0s"),
    ] {
        assert_eq!(go_duration(secs), want);
    }
}

fn argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

#[test]
fn parse_serves_only_the_reproduced_shapes() {
    let q = parse(&argv("42 --repo o/r --json name,bucket,name")).unwrap();
    assert_eq!(
        q,
        Query {
            number: 42,
            repo: Some("o/r".into()),
            json: Some(vec!["bucket".into(), "name".into()]),
        }
    );
    for ok in ["42", "42 -R o/r", "--repo=o/r 42", "42 --json=state", "7 --json link,description"] {
        assert!(parse(&argv(ok)).is_some(), "{ok}");
    }
    for no in [
        "",
        "--json name",
        "42 43",
        "042",
        "feature/x",
        "https://github.com/o/r/pull/1",
        "#42",
        "42 --watch",
        "42 --fail-fast",
        "42 -i 5",
        "42 --interval=5",
        "42 --required",
        "42 --json name --jq .",
        "42 --json name -q .",
        "42 --json name --template x",
        "42 --json event",
        "42 --json workflow,name",
        "42 --json Name",
        "42 --json",
        "42 --json name --json state",
        "42 --repo",
        "42 -R o/r -R o/s",
    ] {
        assert_eq!(parse(&argv(no)), None, "{no}");
    }
}
