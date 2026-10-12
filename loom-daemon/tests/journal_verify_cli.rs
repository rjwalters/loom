//! CLI contract for `loom-daemon journal verify` (#11345), end to end
//! through the real binary on temporary roots.
//!
//! | Exit | Meaning |
//! |---|---|
//! | `0` | every stream readable and clean |
//! | `1` | anomalies (corruption, gap/regression, unknown major, unreadable stream) |
//! | `2` | nothing verifiable (missing / unreadable / empty root) — never green |

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write;
use std::process::{Command, Output};

use loom_daemon::journal::Journal;
use serde_json::json;
use tempfile::TempDir;

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("journal")
        .arg("verify")
        .args(args)
        .env_remove("LOOM_JOURNAL_ROOT")
        .output()
        .unwrap()
}

fn seeded() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    for i in 0..3 {
        stream.append("k", None, json!(i)).unwrap();
    }
    tmp
}

fn root(tmp: &TempDir) -> &str {
    tmp.path().to_str().unwrap()
}

#[test]
fn clean_journal_exits_zero_and_json_parses() {
    let tmp = seeded();
    let out = run(&["--root", root(&tmp)]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));

    let out = run(&["--root", root(&tmp), "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["streams"][0]["records"], json!(3));
}

#[test]
fn corrupted_journal_exits_nonzero() {
    let tmp = seeded();
    let segment = tmp.path().join("s").join("0000000001.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(b"{not json}\n")
        .unwrap();
    let out = run(&["--root", root(&tmp), "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["streams"][0]["corrupt_lines"], json!(1));
}

#[test]
fn missing_or_empty_root_is_never_green() {
    let tmp = TempDir::new().unwrap();
    let out = run(&["--root", root(&tmp)]);
    assert_eq!(out.status.code(), Some(2), "empty root");
    let absent = tmp.path().join("absent");
    let out = run(&["--root", absent.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "missing root");
}

#[test]
fn root_is_required() {
    let out = run(&[]);
    assert!(!out.status.success());
}

#[test]
fn unknown_stream_is_not_green() {
    let tmp = seeded();
    let out = run(&["--root", root(&tmp), "--stream", "absent"]);
    assert_eq!(out.status.code(), Some(1));
}
