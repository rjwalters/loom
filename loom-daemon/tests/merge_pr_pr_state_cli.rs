//! CLI-level check of `loom-daemon merge-pr pr-state`: the protocol line the
//! shell matches, for each row of the retired `merge-pr.sh` decision table.

use std::process::Command;

fn run(state: &str, merged: &str) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["merge-pr", "pr-state", "--state", state, "--merged", merged])
        .output()
        .expect("spawn loom-daemon");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success())
}

#[test]
fn verbs_answer_one_protocol_line_and_exit_zero() {
    for (state, merged, want) in [
        ("closed", "true", "LOOM-PR-STATE MERGED"),
        ("open", "true", "LOOM-PR-STATE MERGED"),
        ("closed", "false", "LOOM-PR-STATE CLOSED"),
        ("closed", "null", "LOOM-PR-STATE CLOSED"),
        ("open", "false", "LOOM-PR-STATE OPEN"),
        ("null", "null", "LOOM-PR-STATE OPEN"),
        ("Closed", "false", "LOOM-PR-STATE OPEN"),
    ] {
        let (stdout, ok) = run(state, merged);
        assert!(ok, "{state}/{merged} exited non-zero");
        assert_eq!(stdout, format!("{want}\n"), "{state}/{merged}");
    }
}
