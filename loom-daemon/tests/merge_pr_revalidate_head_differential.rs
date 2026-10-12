//! Differential test: `loom-daemon merge-pr revalidate-head` against the retired
//! shell it replaced — the jq-filter-and-test decision in `merge-pr.sh`'s
//! `_revalidate_merge_guards` (#8410/#8896).
//!
//! One once-generated corpus (verification-recipes §6) feeds both sides:
//! payload shapes a forge sends (merged, moved, unchanged, no labels, odd label
//! entries) plus unreadable ones (empty, `{}`, not JSON, null/empty SHA).
//! Requires `jq` on PATH, like the shell it freezes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-revalidate-head-retired.sh")
}

fn run_shell(payload: &str, pre: &str) -> String {
    let out = Command::new("bash")
        .args([
            "-c",
            "source \"$1\"; _retired_revalidate \"$2\" \"$3\"",
            "driver",
        ])
        .arg(fixture_path())
        .args([payload, pre])
        .output()
        .expect("bash ran the frozen shell side");
    assert!(out.status.success(), "frozen shell failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn run_rust(payload: &str, pre: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["merge-pr", "revalidate-head", "--precondition-sha", pre])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("loom-daemon spawned");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "revalidate-head exited {:?}", out.status.code());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let corpus = [
        "",
        "{}",
        "not json",
        "null",
        r#"{"merged":true}"#,
        r#"{"merged":true,"head":{"sha":"ab58dd87d"}}"#,
        r#"{"merged":false,"head":{"sha":"490b79f1d"},"labels":[{"name":"loom:pr"}]}"#,
        r#"{"merged":false,"head":{"sha":"ab58dd87d"},"labels":[{"name":"loom:pr"}]}"#,
        r#"{"head":{"sha":"490b79f1d"}}"#,
        r#"{"head":{"sha":"490b79f1d"},"labels":[]}"#,
        r#"{"head":{"sha":"490b79f1d"},"labels":[{"name":"a"},{"name":"b c"},{"name":null},{}]}"#,
        r#"{"head":{"sha":null},"labels":[{"name":"loom:pr"}]}"#,
        r#"{"head":{"sha":""},"labels":[{"name":"loom:pr"}]}"#,
        r#"{"head":{"sha":false}}"#,
        r#"{"head":{"sha":"490b79f1d"},"labels":null}"#,
        r#"{"merged":"true","head":{"sha":"x"}}"#,
        r#"{"merged":null,"head":{"sha":"490b79f1d"},"labels":[{"name":"loom:pr"}]}"#,
    ];
    for payload in corpus {
        for pre in ["490b79f1d", ""] {
            assert_eq!(
                run_shell(payload, pre),
                run_rust(payload, pre),
                "diverged: payload={payload:?} pre={pre:?}"
            );
        }
    }
}
