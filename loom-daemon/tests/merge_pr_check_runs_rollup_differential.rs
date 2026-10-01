//! Differential test: the Rust port of `merge-pr.sh`'s check-runs rollup read
//! (`failing` / `pending` / `total_count` inside
//! `_wait_for_checks_then_sync_merge`) against the frozen retired `jq` filters
//! (`defaults/docs/verification-recipes.md` §6 — the #8191 slice that moved
//! them into `loom_daemon::merge_pr::check_runs_rollup`).
//!
//! # Both sides are the real thing
//!
//! The shell side sources `tests/fixtures/merge-pr-check-runs-rollup-retired.sh`
//! and runs the frozen block with a `jq` wrapper that records whether any of
//! the three `jq` calls FAILED (the retired `|| true` / `|| echo 0` hid that).
//! The Rust side runs the real `loom-daemon merge-pr check-runs-rollup`
//! binary with the payload on stdin, exactly as the live `merge-pr.sh` does,
//! and parses its NUL-framed answer — so the framing, the sentinel and the
//! kernel are all under test.
//!
//! # Every difference must belong to a class recognised by MECHANISM
//!
//! The port answers only for a payload inside the `forge_get_check_runs`
//! contract. Whether a case is inside it is decided HERE, independently of
//! the port: by `serde_json` (exactly one document) plus a `jq` schema filter
//! ([`CONTRACT_JQ`]). Then:
//!
//! - inside the contract, the port must answer and all three fields must equal
//!   the retired output byte for byte — and the retired side must not have hit
//!   a `jq` error (if it did, the contract check itself is wrong);
//! - outside it, the port must refuse (exit 2, empty stdout), and the case is
//!   counted in one of two classes by what the RETIRED side did:
//!   - `retired_jq_faulted`: a `jq` call failed and `|| true` turned it into
//!     an answer — the unreadable-reads-as-settled class the port closes;
//!   - `retired_answered`: the retired filters ran cleanly but the payload
//!     breaks the forge contract (a `null` element, a non-string name, a
//!     non-integer `total_count`) — the port's strictness, fail-safe because
//!     the shell reads a refusal as "still pending".
//!
//! # What the corpus deliberately leaves out
//!
//! `total_count` literals that are integral but not written as plain digits
//! (`5.0`, `1e2`, `-0`) and integers above 2^53: jq 1.6 and jq 1.7 render them
//! differently, so no single "retired answer" exists across hosts. The port
//! refuses the non-plain forms (unit-tested in
//! `merge_pr::check_runs_rollup::tests::total_count_must_be_an_unsigned_integer_literal`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-check-runs-rollup-retired.sh")
}

/// The forge contract, stated independently of the port.
const CONTRACT_JQ: &str = r#"type == "object"
  and (.total_count | type == "number" and . >= 0 and . == floor)
  and (.check_runs | type == "array")
  and all(.check_runs[]; type == "object"
        and (.name | type == "null" or (type == "string" and (explode | any(. == 0) | not))))"#;

struct Retired {
    fields: [Vec<u8>; 3],
    jq_faulted: bool,
}

fn split_nul(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = bytes.split(|b| *b == 0).map(<[u8]>::to_vec).collect();
    assert_eq!(parts.pop().as_deref(), Some(&b""[..]), "frame must be NUL-terminated");
    parts
}

fn run_retired(raw: &str) -> Retired {
    let driver = r#"set -uo pipefail
fault_log="$(mktemp)"
jq() { command jq "$@" || { echo fault >>"$fault_log"; return 1; }; }
source "$1"
_retired_check_runs_rollup "$2"
if [[ -s "$fault_log" ]]; then printf 'FAULT\0'; else printf 'CLEAN\0'; fi
rm -f "$fault_log"
"#;
    let out = Command::new("bash")
        .args(["-c", driver, "driver"])
        .arg(fixture_path())
        .arg(raw)
        .env("LC_ALL", "C")
        .output()
        .expect("bash ran the frozen shell side");
    assert!(
        out.status.success(),
        "retired side failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parts = split_nul(&out.stdout);
    assert_eq!(parts.len(), 4, "retired frame: {:?}", String::from_utf8_lossy(&out.stdout));
    Retired {
        jq_faulted: parts[3] == b"FAULT",
        fields: [parts[0].clone(), parts[1].clone(), parts[2].clone()],
    }
}

/// `Ok(fields)` when the port answered, `Err(())` when it refused.
fn run_port(raw: &str) -> Result<[Vec<u8>; 3], ()> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["merge-pr", "check-runs-rollup"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("loom-daemon spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    match out.status.code() {
        Some(0) => {
            let parts = split_nul(&out.stdout);
            assert_eq!(parts.len(), 4, "port frame for {raw:?}");
            assert_eq!(parts[3], b"LOOM-CHECK-RUNS-ROLLUP", "sentinel for {raw:?}");
            Ok([parts[0].clone(), parts[1].clone(), parts[2].clone()])
        }
        Some(2) => {
            assert!(out.stdout.is_empty(), "a refusal must print nothing on stdout: {raw:?}");
            Err(())
        }
        other => panic!(
            "unexpected exit {other:?} for {raw:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

fn inside_contract(raw: &str) -> bool {
    let mut docs = serde_json::Deserializer::from_str(raw).into_iter::<serde_json::Value>();
    let one_doc = matches!(docs.next(), Some(Ok(_))) && docs.next().is_none();
    if !one_doc {
        return false;
    }
    let mut child = Command::new("jq")
        .args(["-e", CONTRACT_JQ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("jq spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(raw.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    out.status.success() && out.stdout == b"true\n"
}

fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

/// A check-run object from optional pre-encoded JSON field values.
fn run(name: Option<&str>, status: Option<&str>, conclusion: Option<&str>) -> String {
    let mut fields = Vec::new();
    for (k, v) in [
        ("name", name),
        ("status", status),
        ("conclusion", conclusion),
    ] {
        if let Some(v) = v {
            fields.push(format!("\"{k}\":{v}"));
        }
    }
    format!("{{{}}}", fields.join(","))
}

fn rollup(total: &str, runs: &[String]) -> String {
    format!("{{\"total_count\":{total},\"check_runs\":[{}]}}", runs.join(","))
}

/// The corpus, enumerated from the payload grammar — no PRNG.
fn corpus() -> Vec<String> {
    let names: Vec<Option<String>> = [
        "A",
        "B",
        "a",
        "",
        " ",
        "null",
        "a\nb",
        "z\n\n",
        "\nlead",
        "é",
        "build (ubuntu-latest, stable)",
        "a\u{0}b",
    ]
    .iter()
    .map(|s| Some(js(s)))
    .chain([
        None,
        Some("null".into()),
        Some("7".into()),
        Some("[\"A\"]".into()),
        Some("true".into()),
    ])
    .collect();
    let statuses: Vec<Option<String>> = ["completed", "in_progress", "queued", "Completed"]
        .iter()
        .map(|s| Some(js(s)))
        .chain([None, Some("null".into()), Some("1".into())])
        .collect();
    let conclusions: Vec<Option<String>> = [
        "failure",
        "timed_out",
        "cancelled",
        "action_required",
        "success",
        "neutral",
        "skipped",
        "Failure",
        "failure ",
    ]
    .iter()
    .map(|s| Some(js(s)))
    .chain([None, Some("null".into()), Some("true".into())])
    .collect();

    let mut out = Vec::new();
    // Every status x conclusion, one named run.
    for s in &statuses {
        for c in &conclusions {
            out.push(rollup("1", &[run(Some("\"X\""), s.as_deref(), c.as_deref())]));
        }
    }
    // Every name, as a failing run and as a pending run.
    let failing = js("failure");
    let completed = js("completed");
    let queued = js("queued");
    for n in &names {
        out.push(rollup("1", &[run(n.as_deref(), Some(&completed), Some(&failing))]));
        out.push(rollup("1", &[run(n.as_deref(), Some(&queued), None)]));
        out.push(rollup("1", &[run(n.as_deref(), Some(&completed), Some(&js("success")))]));
    }
    // Ordered pairs of names (sort, de-dup, null-vs-"null", newline joins),
    // both as two pending runs and as two failing runs. Only names inside the
    // contract: the out-of-contract ones are refused on their own above, and
    // pairing them would only multiply refusals.
    let pair_names: Vec<&Option<String>> = names
        .iter()
        .filter(|n| {
            n.as_deref()
                .is_none_or(|v| v == "null" || (v.starts_with('"') && !v.contains("\\u0000")))
        })
        .collect();
    for a in &pair_names {
        for b in &pair_names {
            out.push(rollup(
                "2",
                &[
                    run(a.as_deref(), Some(&queued), None),
                    run(b.as_deref(), Some(&queued), None),
                ],
            ));
            out.push(rollup(
                "2",
                &[
                    run(a.as_deref(), Some(&completed), Some(&failing)),
                    run(b.as_deref(), Some(&completed), Some(&failing)),
                ],
            ));
        }
    }
    // A mixed rollup: failing, pending, green, and a failed-yet-running run.
    out.push(rollup(
        "4",
        &[
            run(Some("\"Lint\""), Some(&completed), Some(&failing)),
            run(Some("\"Build\""), Some(&js("in_progress")), Some("null")),
            run(Some("\"Docs\""), Some(&completed), Some(&js("success"))),
            run(Some("\"Flaky\""), Some(&js("in_progress")), Some(&failing)),
        ],
    ));
    // total_count values (see the module docs for what is left out).
    for total in [
        "0",
        "1",
        "2",
        "39",
        "100",
        "4294967296",
        "9007199254740991",
        "-1",
        "2.5",
        "\"5\"",
        "null",
        "true",
        "[1]",
        "{}",
    ] {
        out.push(rollup(total, &[run(Some("\"A\""), Some(&queued), None)]));
        out.push(rollup(total, &[]));
    }
    // Element and document shapes.
    for elem in ["null", "\"A\"", "1", "[]", "true", "{}"] {
        out.push(rollup("1", &[elem.to_string()]));
    }
    let a_pending = run(Some("\"A\""), Some(&queued), None);
    for doc in [
        String::new(),
        "   ".into(),
        "null".into(),
        "[]".into(),
        "\"x\"".into(),
        "7".into(),
        "not json".into(),
        "{\"check_runs\":[]}".into(),
        format!("{{\"check_runs\":[{a_pending}]}}"),
        "{\"total_count\":1}".into(),
        "{\"total_count\":1,\"check_runs\":null}".into(),
        format!("{{\"total_count\":1,\"check_runs\":{{\"k\":{a_pending}}}}}"),
        "{\"total_count\":1,\"check_runs\":\"A\"}".into(),
        format!("{} trailing", rollup("1", std::slice::from_ref(&a_pending))),
        format!("{}{}", rollup("1", std::slice::from_ref(&a_pending)), rollup("1", std::slice::from_ref(&a_pending))),
        format!(" \n{}\n", rollup("1", std::slice::from_ref(&a_pending))),
        format!("{{\"total_count\":1,\"total_count\":2,\"check_runs\":[{a_pending}],\"extra\":[1]}}"),
        "{\"total_count\":1,\"check_runs\":[{\"name\":\"A\",\"name\":\"B\",\"status\":\"queued\"}]}".into(),
    ] {
        out.push(doc);
    }
    out
}

#[test]
fn frozen_shell_and_rust_cli_agree_or_differ_only_by_named_class() {
    let cases = corpus();
    let (mut answered, mut retired_jq_faulted, mut retired_answered) = (0, 0, 0);
    let mut false_settle_closed = 0;
    let mut saw = [false; 5]; // failing, pending, null-name, newline-name, multi-line
    for raw in &cases {
        let retired = run_retired(raw);
        let port = run_port(raw);
        if inside_contract(raw) {
            assert!(
                !retired.jq_faulted,
                "contract check admitted a payload the retired jq could not read: {raw:?}"
            );
            let fields =
                port.unwrap_or_else(|()| panic!("port refused an in-contract payload: {raw:?}"));
            for (i, label) in ["failing", "pending", "total_count"].iter().enumerate() {
                assert_eq!(
                    String::from_utf8_lossy(&fields[i]),
                    String::from_utf8_lossy(&retired.fields[i]),
                    "{label} diverges on {raw:?}"
                );
            }
            answered += 1;
            saw[0] |= !fields[0].is_empty();
            saw[1] |= !fields[1].is_empty();
            saw[2] |= raw.contains("\"name\":null") && fields[1].starts_with(b"null");
            saw[3] |= raw.contains("a\\nb") && fields[1].windows(3).any(|w| w == b"a\nb");
            saw[4] |= fields[1].iter().filter(|b| **b == b'\n').count() >= 2;
        } else {
            assert!(port.is_err(), "port answered a payload outside the forge contract: {raw:?}");
            if retired.jq_faulted {
                retired_jq_faulted += 1;
                // The class this slice closes: the retired side read the
                // unreadable payload as "nothing failing, nothing pending".
                if retired.fields[0].is_empty() && retired.fields[1].is_empty() {
                    false_settle_closed += 1;
                }
            } else {
                retired_answered += 1;
            }
        }
    }
    eprintln!(
        "{} cases: {answered} answered identically, {retired_jq_faulted} refused where retired jq faulted \
         ({false_settle_closed} of them read as settled by the retired side), {retired_answered} refused \
         where retired answered",
        cases.len()
    );
    assert!(answered >= 200, "too few in-contract cases: {answered}");
    assert!(retired_jq_faulted >= 10, "too few retired-jq-fault cases: {retired_jq_faulted}");
    assert!(
        false_settle_closed >= 5,
        "the false-settle class was never exercised: {false_settle_closed}"
    );
    assert!(retired_answered >= 10, "too few strict-contract refusals: {retired_answered}");
    assert!(
        saw.iter().all(|s| *s),
        "coverage floor not met (failing, pending, null-name, newline-name, multi-line): {saw:?}"
    );
}
