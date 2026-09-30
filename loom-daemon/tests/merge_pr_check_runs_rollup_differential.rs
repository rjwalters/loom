//! Differential test: the Rust port of `merge-pr.sh`'s check-runs rollup
//! parse must agree with the shell it replaced, byte for byte, on a shared
//! corpus (`defaults/docs/verification-recipes.md` §6 — the #8191 slice that
//! moved the three `jq` filters at the top of
//! `_wait_for_checks_then_sync_merge`'s poll into
//! `loom_daemon::merge_pr::check_runs_rollup`).
//!
//! # The corpus is fed to both sides
//!
//! The shell side runs the REAL frozen block, sourced from
//! `tests/fixtures/merge-pr-check-runs-rollup-retired.sh`, which renders the
//! four things the rest of the poll consumed: `total_count` after the
//! `^[0-9]+$` gate, the two `[[ -n … ]]` branch tests, the `wc -l` count the
//! loop narrates, and the two newline-joined name strings that become the NUL
//! frame `merge-pr checks-failure` reads. The Rust side runs the REAL
//! `loom-daemon merge-pr check-runs-rollup` binary on the same payload and
//! renders its records into the same shape — so the sentinel protocol and the
//! record framing are under test too, not just
//! [`loom_daemon::merge_pr::check_runs_rollup::parse`].
//!
//! # Why this needs a differential at all
//!
//! All three retired filters were `jq` one-liners written `2>/dev/null ||
//! true`, and their whole bug surface is what `jq` did with a shape nobody
//! anticipated: `unique` sorts as well as de-duplicates, `jq -r` unquotes
//! strings ONLY (so a check-run with no `name` is the literal text `null`,
//! not a dropped row), `.[]` over an OBJECT iterates its values, a type error
//! on ONE row aborts the whole filter and takes the rows already collected
//! with it, and `//` fires on `false` as well as `null`. Each of those is a
//! case where a plausible hand-rewrite silently changes which checks the wait
//! loop thinks are running — and a shorter pending set is the direction that
//! ends a wait early.
//!
//! # Collation
//!
//! Both sides run under `LC_ALL=C`. `jq`'s own string ordering is by
//! codepoint and is locale-independent, so the locale only pins bash's
//! `^[0-9]+$` to ASCII digits, which is the comparison the port makes.
//!
//! # Coverage floor
//!
//! The test fails unless the corpus reached a failing-only answer, a
//! pending-only answer, a both-populated answer, the all-empty answer an
//! unwalkable payload produces, and a `total_count` the bash gate rejected.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/merge-pr-check-runs-rollup-retired.sh")
}

/// Run the frozen block and return its rendering verbatim.
fn run_frozen_shell(payload: &str) -> String {
    let driver = r#"set -uo pipefail
source "$1"
_retired_parse_rollup "$2"
"#;
    let out = Command::new("bash")
        .args(["-c", driver, "driver"])
        .arg(fixture_path())
        .arg(payload)
        .env("LC_ALL", "C")
        .output()
        .expect("bash ran the frozen shell side");
    assert_eq!(
        out.status.code(),
        Some(0),
        "frozen shell side exited nonzero for {payload:?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Only the trailing NEWLINE is stripped, never trailing whitespace: the
    // last rendered field is a joined name string that is legitimately empty
    // (or ends in a space), and `trim_end` would erase the very tab that says
    // so.
    String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// Run the ported subcommand exactly as the live `merge-pr.sh` does, then
/// rebuild the retired rendering from its records — including the shell's own
/// re-joining loop, so a framing mistake shows up here rather than only in
/// production.
fn run_rust_cli(payload: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["merge-pr", "check-runs-rollup"])
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("loom-daemon spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "check-runs-rollup exited nonzero for {payload:?}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    let mut header: Option<Vec<String>> = None;
    let mut failing: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let (kind, rest) = line.split_once('\t').unwrap_or((line, ""));
        match kind {
            "LOOM-CHECK-RUNS-ROLLUP" => {
                header = Some(rest.split('\t').map(str::to_string).collect());
            }
            "FAILING" => failing.push(rest.to_string()),
            "PENDING" => pending.push(rest.to_string()),
            other => panic!("unrecognized record {other:?} for {payload:?}"),
        }
    }
    let header = header.unwrap_or_else(|| panic!("no sentinel header for {payload:?}"));
    assert_eq!(header.len(), 4, "header field count for {payload:?}: {header:?}");
    format!(
        "TOTAL\t{}\nANY\t{}/{}\nLINES\t{}\nFJOIN\t{}\nPJOIN\t{}",
        header[0],
        header[1],
        header[2],
        header[3],
        failing.join("|"),
        pending.join("|"),
    )
}

/// Every payload is fed to both sides unchanged; the list is generated once
/// and never regenerated per-side.
fn corpus() -> Vec<String> {
    let mut cases: Vec<String> = vec![
        // --- the ordinary shapes the wait loop meets every day ---
        r#"{"total_count":0,"check_runs":[]}"#.into(),
        r#"{"total_count":1,"check_runs":[{"name":"Required Build","status":"completed","conclusion":"success"}]}"#.into(),
        r#"{"total_count":1,"check_runs":[{"name":"Deploy Preview","status":"queued","conclusion":null}]}"#.into(),
        r#"{"total_count":2,"check_runs":[{"name":"Required Build","status":"in_progress","conclusion":null},{"name":"Lint","status":"completed","conclusion":"success"}]}"#.into(),
        r#"{"total_count":1,"check_runs":[{"name":"Required Build","status":"completed","conclusion":"failure"}]}"#.into(),
        r#"{"total_count":2,"check_runs":[{"name":"Flaky Job","status":"completed","conclusion":"failure"},{"name":"Required Build","status":"in_progress","conclusion":null}]}"#.into(),
        // --- `unique` SORTS and de-duplicates (jq order, not rollup order) ---
        r#"{"total_count":4,"check_runs":[{"name":"zeta","status":"queued"},{"name":"Alpha","status":"queued"},{"name":"zeta","status":"in_progress"},{"name":"beta","status":"queued"}]}"#.into(),
        r#"{"total_count":3,"check_runs":[{"name":"b","status":"completed","conclusion":"failure"},{"name":"B","status":"completed","conclusion":"timed_out"},{"name":"a","status":"completed","conclusion":"cancelled"}]}"#.into(),
        // --- `jq -r` unquotes STRINGS only: a non-string name is text ---
        r#"{"total_count":2,"check_runs":[{"name":"Lint","status":"queued"},{"status":"queued"}]}"#.into(),
        r#"{"total_count":4,"check_runs":[{"name":"CI","status":"queued"},{"name":7,"status":"queued"},{"name":true,"status":"queued"},{"name":null,"status":"queued"}]}"#.into(),
        // --- an EMPTY name: one row, but the joined string is empty, which is
        //     what `[[ -n "$pending" ]]` measured ---
        r#"{"total_count":1,"check_runs":[{"name":"","status":"queued"}]}"#.into(),
        r#"{"total_count":2,"check_runs":[{"name":"","status":"queued"},{"name":"CI","status":"queued"}]}"#.into(),
        r#"{"total_count":1,"check_runs":[{"name":"","status":"completed","conclusion":"failure"}]}"#.into(),
        // --- shapes `jq` could not walk: EMPTY answer, never an error ---
        r#"{"total_count":3,"check_runs":null}"#.into(),
        r#"{"total_count":0}"#.into(),
        r#"{"total_count":2,"check_runs":[{"name":"Lint","status":"queued"},7]}"#.into(),
        r#"{"total_count":1,"check_runs":["Lint"]}"#.into(),
        r#"{"total_count":1,"check_runs":[[]]}"#.into(),
        r#"{"total_count":1,"check_runs":"nope"}"#.into(),
        r#"{"total_count":5,"check_runs":[7]}"#.into(),
        // …but a NULL row IS indexable in jq, and contributes a `null` name
        r#"{"total_count":1,"check_runs":[null]}"#.into(),
        // …and an OBJECT `check_runs` iterates its VALUES
        r#"{"total_count":1,"check_runs":{"a":{"name":"CI","status":"queued"}}}"#.into(),
        // --- unparseable / absent payloads ---
        String::new(),
        "   ".into(),
        "not json".into(),
        r#"{"check_runs":["#.into(),
        "null".into(),
        r#"[{"name":"CI","status":"queued"}]"#.into(),
        r#"{} {}"#.into(),
        // --- `.total_count // 0` plus bash's `^[0-9]+$` gate ---
        r#"{"total_count":null,"check_runs":[]}"#.into(),
        r#"{"total_count":false,"check_runs":[]}"#.into(),
        r#"{"total_count":true,"check_runs":[]}"#.into(),
        r#"{"total_count":"7","check_runs":[]}"#.into(),
        r#"{"total_count":7.0,"check_runs":[]}"#.into(),
        r#"{"total_count":-1,"check_runs":[]}"#.into(),
        r#"{"total_count":"abc","check_runs":[]}"#.into(),
        r#"{"total_count":"","check_runs":[]}"#.into(),
        r#"{"total_count":" 7","check_runs":[]}"#.into(),
        r#"{"total_count":"7 ","check_runs":[]}"#.into(),
        r#"{"total_count":"7\n8","check_runs":[]}"#.into(),
        r#"{"total_count":[],"check_runs":[]}"#.into(),
        r#"{"total_count":18446744073709551615,"check_runs":[]}"#.into(),
        // NOTE: an integer `total_count` above u64::MAX is deliberately NOT in
        // this corpus — jq >= 1.7 preserves its literal digits while this port
        // (and jq <= 1.6) renders it in exponent form, which the `^[0-9]+$`
        // gate rejects. The bound is argued in the module docs; a rollup
        // counting more check runs than u64 can hold is not a reachable state.
    ];

    // Every conclusion GitHub can report, against the terminal-failing four.
    for c in [
        "failure",
        "timed_out",
        "cancelled",
        "action_required",
        "success",
        "skipped",
        "neutral",
        "stale",
        "startup_failure",
        "FAILURE",
        "Failure",
        "failures",
        "pre-failure",
        "timedout",
    ] {
        cases.push(format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"completed","conclusion":"{c}"}}]}}"#
        ));
    }
    // …and every status, against the single-value `!= "completed"` denylist.
    for s in [
        "completed",
        "queued",
        "in_progress",
        "waiting",
        "pending",
        "requested",
        "COMPLETED",
    ] {
        cases.push(format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"{s}","conclusion":null}}]}}"#
        ));
    }
    // Workflow job names with the punctuation GitHub's matrix syntax produces,
    // plus a name carrying the `|` the rendering uses as its separator (both
    // sides render it identically, so the comparison still holds).
    for n in [
        "build (ubuntu-latest, stable)",
        "a\\tb",
        "a|b",
        "  lead space",
        "Ünïcode",
    ] {
        cases.push(format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"{n}","status":"queued"}}]}}"#
        ));
    }
    cases
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let mut saw_failing_only = false;
    let mut saw_pending_only = false;
    let mut saw_both = false;
    let mut saw_all_empty = false;
    let mut saw_gated_total = false;

    for payload in corpus() {
        let shell_out = run_frozen_shell(&payload);
        let rust_out = run_rust_cli(&payload);
        assert_eq!(shell_out, rust_out, "divergence on payload {payload:?}");

        let any = rust_out
            .lines()
            .find_map(|l| l.strip_prefix("ANY\t"))
            .expect("rendering carries an ANY line");
        match any {
            "1/0" => saw_failing_only = true,
            "0/1" => saw_pending_only = true,
            "1/1" => saw_both = true,
            _ => {}
        }
        if rust_out.starts_with("TOTAL\t0\n") && rust_out.ends_with("FJOIN\t\nPJOIN\t") {
            saw_all_empty = true;
        }
        // A payload whose `.total_count` was readable but non-numeric: the
        // bash gate substituted 0 where the raw value was not all-digits.
        if payload.contains(r#""total_count":"abc""#) && rust_out.starts_with("TOTAL\t0\n") {
            saw_gated_total = true;
        }
    }

    assert!(saw_failing_only, "corpus never reached a failing-only rollup");
    assert!(saw_pending_only, "corpus never reached a pending-only rollup");
    assert!(saw_both, "corpus never reached a rollup with both sets populated");
    assert!(
        saw_all_empty,
        "corpus never reached the all-empty answer an unwalkable payload gives"
    );
    assert!(
        saw_gated_total,
        "corpus never exercised the ^[0-9]+$ gate rejecting a total_count"
    );
}
