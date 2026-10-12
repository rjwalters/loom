//! Differential test: `loom-daemon merge-pr poll-wait` against the retired
//! shell it replaced - the unfetchable-check-runs and pending-checks arms of
//! `_wait_for_checks_then_sync_merge` (#8191 slice).
//!
//! One once-generated corpus (verification-recipes section 6) feeds both
//! sides: the clock at, just under and just over the deadline; one, several and
//! many pending names; adversarial pending text (an embedded blank line, no
//! trailing newline in the name list, non-ASCII), and odd timeout / interval /
//! rc spellings that are only ever interpolated.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/merge-pr-poll-wait-retired.sh")
}

struct Case<'a> {
    pending_arm: bool,
    now: i64,
    deadline: i64,
    timeout: &'a str,
    interval: &'a str,
    pr: &'a str,
    /// `rc` for the unfetchable arm, the pending list for the pending arm.
    extra: &'a str,
}

fn run_shell(c: &Case) -> String {
    let func = if c.pending_arm {
        "_retired_pending_arm"
    } else {
        "_retired_unfetchable_arm"
    };
    let out = Command::new("bash")
        .args([
            "-c",
            &format!("source \"$1\"; shift; {func} \"$@\""),
            "driver",
        ])
        .arg(fixture_path())
        .args([
            c.now.to_string().as_str(),
            c.deadline.to_string().as_str(),
            c.timeout,
            c.interval,
            c.pr,
            c.extra,
        ])
        .output()
        .expect("bash ran the frozen shell side");
    assert!(out.status.success(), "frozen shell failed");
    String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .to_string()
}

fn run_rust(c: &Case) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["merge-pr", "poll-wait", "--kind"])
        .arg(if c.pending_arm {
            "pending"
        } else {
            "unfetchable"
        })
        .args(["--pr", c.pr, "--now"])
        .arg(c.now.to_string())
        .arg("--deadline")
        .arg(c.deadline.to_string())
        .args(["--timeout", c.timeout, "--interval", c.interval]);
    if !c.pending_arm {
        cmd.args(["--rc", c.extra]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("loom-daemon spawned");
    // The shell pipes `printf '%s\n' "$pending"`.
    let payload = if c.pending_arm {
        format!("{}\n", c.extra)
    } else {
        String::new()
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "poll-wait exited {:?}", out.status.code());
    let line = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches('\n')
        .to_string();
    // `LOOM-POLL-WAIT <ACTION> <level> <message>` -> the frozen `<ACTION> <level> <message>`.
    line.strip_prefix("LOOM-POLL-WAIT ")
        .expect("sentinel")
        .to_string()
}

#[test]
fn frozen_shell_and_rust_cli_agree_byte_for_byte() {
    let clocks = [
        (0, 100),
        (99, 100),
        (100, 100),
        (101, 100),
        (1_700_000_100, 1_700_000_100),
    ];
    let knobs = [("600", "30"), ("0", "1"), ("abc", "x y")];
    let prs = ["42", "100000"];
    let pendings = [
        "build",
        "build\ntest",
        "a\nb\nc\nd\ne",
        "build\n\ntest",
        "caf\u{e9} check\nsecond",
        "-n",
        "name with 'quote' and \"dq\"",
    ];
    let rcs = ["1", "44", "45", "0", "-3", "abc"];
    let (mut waits, mut timeouts, mut count) = (0, 0, 0);
    for (now, deadline) in clocks {
        for (timeout, interval) in knobs {
            for pr in prs {
                for p in pendings {
                    let c = Case {
                        pending_arm: true,
                        now,
                        deadline,
                        timeout,
                        interval,
                        pr,
                        extra: p,
                    };
                    let (s, r) = (run_shell(&c), run_rust(&c));
                    assert_eq!(s, r, "pending diverged: now={now} dl={deadline} to={timeout} iv={interval} pr={pr} p={p:?}");
                    if r.starts_with("WAIT") {
                        waits += 1
                    } else {
                        timeouts += 1
                    }
                    count += 1;
                }
                for rc in rcs {
                    let c = Case {
                        pending_arm: false,
                        now,
                        deadline,
                        timeout,
                        interval,
                        pr,
                        extra: rc,
                    };
                    let (s, r) = (run_shell(&c), run_rust(&c));
                    assert_eq!(s, r, "unfetchable diverged: now={now} dl={deadline} to={timeout} pr={pr} rc={rc}");
                    if r.starts_with("WAIT") {
                        waits += 1
                    } else {
                        timeouts += 1
                    }
                    count += 1;
                }
            }
        }
    }
    assert!(
        waits > 50 && timeouts > 50,
        "coverage floor: waits={waits} timeouts={timeouts} of {count}"
    );
}
