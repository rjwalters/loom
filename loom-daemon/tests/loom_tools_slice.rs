//! `loom-tools` slice parity and mode gate (#12938, design
//! `docs/design/loom-tools-cli-boundary.md` §7.3, §10.2, §10.3).
//!
//! Runs the real `loom-tools` and `loom-daemon` binaries on the same inputs and
//! compares stdout and exit codes. The §7.3 resolution table itself is covered
//! by the unit tests in `src/tools_mode.rs`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

const TOOLS: &str = env!("CARGO_BIN_EXE_loom-tools");
const DAEMON: &str = env!("CARGO_BIN_EXE_loom-daemon");

/// A clean environment: no inherited mode, routing or forge credential.
fn base(bin: &str, home: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(bin);
    c.current_dir(cwd)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LOOM_MODE", "session");
    c
}

fn run(mut c: Command, stdin: Option<&str>) -> Output {
    c.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

/// Run both binaries with the same args/stdin/cwd and assert identical stdout
/// and exit code; returns the tools output.
fn parity(args: &[&str], stdin: Option<&str>, cwd: &Path, home: &Path) -> Output {
    let t = run(
        {
            let mut c = base(TOOLS, home, cwd);
            c.args(args);
            c
        },
        stdin,
    );
    let d = run(
        {
            let mut c = base(DAEMON, home, cwd);
            c.args(args);
            c
        },
        stdin,
    );
    assert_eq!(t.status.code(), d.status.code(), "exit code differs for {args:?}");
    assert_eq!(
        String::from_utf8_lossy(&t.stdout),
        String::from_utf8_lossy(&d.stdout),
        "stdout differs for {args:?}"
    );
    t
}

#[test]
fn strip_ansi_stdin_and_file_match_loom_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let input = "\x1b[31mred\x1b[0m line\nplain\nplain\n\x1b[2K\rspinner\n";
    parity(&["strip-ansi"], Some(input), tmp.path(), tmp.path());
    let f = tmp.path().join("log.txt");
    std::fs::write(&f, input).unwrap();
    let out = parity(&["strip-ansi", "--file", f.to_str().unwrap()], None, tmp.path(), tmp.path());
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains('\x1b'));
}

#[test]
fn checkpoint_stages_match_loom_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    parity(&["checkpoint", "stages"], None, tmp.path(), tmp.path());
    let out = parity(&["checkpoint", "stages", "--json"], None, tmp.path(), tmp.path());
    assert!(out.status.success());
}

#[test]
fn checkpoint_write_read_clear_match_loom_daemon() {
    let home = tempfile::tempdir().unwrap();
    let wt_t = tempfile::tempdir().unwrap();
    let wt_d = tempfile::tempdir().unwrap();
    let args = |wt: &Path| {
        vec![
            "checkpoint".to_string(),
            "write".into(),
            "--stage".into(),
            "implementing".into(),
            "--files-changed".into(),
            "3".into(),
            "--worktree".into(),
            wt.display().to_string(),
            "--quiet".into(),
        ]
    };
    for (bin, wt) in [(TOOLS, wt_t.path()), (DAEMON, wt_d.path())] {
        let o = run(
            {
                let mut c = base(bin, home.path(), wt);
                c.args(args(wt));
                c
            },
            None,
        );
        assert_eq!(o.status.code(), Some(0), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
    }
    // Read through each binary, cwd as the worktree (no --worktree flag). The
    // timestamps differ between the two worktrees, so compare with the
    // timestamp and age lines removed.
    let read = |bin: &str, wt: &Path| {
        let o = run(
            {
                let mut c = base(bin, home.path(), wt);
                c.args(["checkpoint", "read"]);
                c
            },
            None,
        );
        assert_eq!(o.status.code(), Some(0), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.contains("timestamp=") && !l.trim_start().starts_with("Age:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (rt, rd) = (read(TOOLS, wt_t.path()), read(DAEMON, wt_d.path()));
    assert_eq!(rt, rd);
    assert!(rt.contains("Files changed: 3"), "{rt}");
    for (bin, wt) in [(TOOLS, wt_t.path()), (DAEMON, wt_d.path())] {
        let o = run(
            {
                let mut c = base(bin, home.path(), wt);
                c.args(["checkpoint", "clear", "--quiet"]);
                c
            },
            None,
        );
        assert_eq!(o.status.code(), Some(0));
    }
    // A missing checkpoint reads the same way from both.
    parity(&["checkpoint", "read", "--json"], None, wt_t.path(), home.path());
}

#[test]
fn checkpoint_read_with_issue_and_no_credential_fails_open() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let o = run(
        {
            let mut c = base(TOOLS, home.path(), wt.path());
            c.args([
                "checkpoint",
                "write",
                "--stage",
                "implementing",
                "--issue",
                "4242",
                "--quiet",
            ]);
            c
        },
        None,
    );
    assert_eq!(o.status.code(), Some(0));
    let o = run(
        {
            let mut c = base(TOOLS, home.path(), wt.path());
            c.args(["checkpoint", "read", "--json"]);
            c
        },
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["issue_state"], "unknown", "{v}");
    assert_eq!(v["stale"], false, "unknown is treated as open: {v}");
}

#[test]
fn checkpoint_read_with_unset_mode_and_fleet_variables_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let mut c = base(TOOLS, home.path(), wt.path());
    c.env_remove("LOOM_MODE")
        .env("LOOM_GH_GATEWAY_DIR", gw.path())
        .args(["checkpoint", "read"]);
    let o = run(c, None);
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("LOOM_GH_GATEWAY_DIR") && err.contains("LOOM_MODE=session"),
        "{err}"
    );
}

#[test]
fn config_get_mode_works_with_mode_unset() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let mut c = base(TOOLS, home.path(), wt.path());
    c.env_remove("LOOM_MODE").args(["config", "get", "mode"]);
    let o = run(c, None);
    assert_eq!(
        o.status.code(),
        Some(1),
        "key unset exits 1: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["found"], false);
}

// ---- #14395: forge reads ------------------------------------------------
//
// `base()` gives a clean install: no `loom-daemon` on PATH, no fleet routing,
// no forge credential. Each case runs the real `loom-tools forge <verb>`.

/// Verbs whose answer is a nonzero exit or a stdin-derived line need no forge
/// at all; both roots must agree on stdout and on the exit code, including
/// exit 1 as data.
#[test]
fn forge_reads_match_loom_daemon_including_exit_codes_as_data() {
    let tmp = tempfile::tempdir().unwrap();
    // `OK` (exit 0) and `REJECT` (exit 1: the data answer) for stdin bodies.
    let ok = parity(
        &["forge", "verdict-body-check"],
        Some("Reviewed the diff and the tests; the change is sound.\n"),
        tmp.path(),
        tmp.path(),
    );
    assert_eq!(ok.status.code(), Some(0));
    let reject = parity(&["forge", "verdict-body-check"], Some("-\n"), tmp.path(), tmp.path());
    assert_eq!(reject.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&reject.stdout).contains("REJECT"));
    // Not a fleet login: exit 1, nothing on stdout.
    let fleet = parity(&["forge", "is-fleet", "some-human"], None, tmp.path(), tmp.path());
    assert_eq!(fleet.status.code(), Some(1));
    assert!(fleet.stdout.is_empty());
    parity(&["forge", "dashboard-link", "acme/widgets", "7"], None, tmp.path(), tmp.path());
    parity(
        &["forge", "dashboard-link", "acme/widgets", "7", "--pr"],
        None,
        tmp.path(),
        tmp.path(),
    );
}

/// A write verb is refused before any handler runs, with a status no read
/// verb uses as data (design §3.1 split; the writes are #14396).
#[test]
fn forge_write_verbs_are_refused_with_exit_64() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        &["forge", "comment", "7", "--body", "x"][..],
        &["forge", "auto-merge", "7"],
        &[
            "forge",
            "merge-queue",
            "enqueue",
            "7",
            "--approved-sha",
            "abc",
        ],
        &["forge", "parent", "link", "--parent", "1", "--child", "2"],
    ] {
        let mut c = base(TOOLS, tmp.path(), tmp.path());
        c.args(args);
        let o = run(c, None);
        assert_eq!(o.status.code(), Some(64), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(o.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("#14396") && err.contains("loom-daemon forge"), "{err}");
    }
}

/// Design §7.3 / §10.3: a forge-reaching read resolves `mode` first. An
/// inherited gateway variable with `mode` unset is the actionable error, not a
/// silently honored (or shed) route.
#[test]
fn forge_read_with_unset_mode_and_fleet_variables_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let mut c = base(TOOLS, home.path(), wt.path());
    c.env_remove("LOOM_MODE")
        .env("LOOM_GH_GATEWAY_DIR", gw.path())
        .args(["forge", "is-fleet", "some-human"]);
    let o = run(c, None);
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("LOOM_GH_GATEWAY_DIR") && err.contains("LOOM_MODE=session"),
        "{err}"
    );
    assert!(o.stdout.is_empty());
    // A verb with no forge call (stdin in, one line out) is not gated.
    let mut c = base(TOOLS, home.path(), wt.path());
    c.env_remove("LOOM_MODE")
        .env("LOOM_GH_GATEWAY_DIR", gw.path())
        .args(["forge", "verdict-body-check"]);
    let o = run(c, Some("-\n"));
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stdout).contains("REJECT"));
}

/// `capabilities --json` is the one capability source: it lists the forge
/// reads and none of the writes, and `install-compat show --json` prints the
/// same list.
#[test]
fn capabilities_list_the_forge_reads_and_not_the_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let t = run(
        {
            let mut c = base(TOOLS, tmp.path(), tmp.path());
            c.args(["capabilities", "--json"]);
            c
        },
        None,
    );
    assert_eq!(t.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&t.stdout).unwrap();
    let cmds: Vec<&str> = v["tools_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    for read in [
        "forge check-claim",
        "forge wait-checks",
        "forge merge-queue status",
        "forge issue",
    ] {
        assert!(cmds.contains(&read), "{read} missing from {cmds:?}");
    }
    for write in [
        "forge comment",
        "forge create-issue",
        "forge auto-merge",
        "forge rerun",
    ] {
        assert!(!cmds.contains(&write), "{write} must not be served: {cmds:?}");
    }
    let d = run(
        {
            let mut c = base(DAEMON, tmp.path(), tmp.path());
            c.args(["install-compat", "show", "--json"]);
            c
        },
        None,
    );
    let dv: serde_json::Value = serde_json::from_slice(&d.stdout).unwrap();
    assert_eq!(dv["tools_commands"], v["tools_commands"]);
}

// ---- #14464: park records and operator decisions --------------------------
//
// Both roots parse the one clap family and run the one in-process handler, so
// these compare the real binaries on stdin/file/argv inputs. `base()` carries
// no forge credential and no `gh` route, which is the point for the offline
// forms: they must not need one.

const DECISION: &str = r#"{"question":"Ship now or wait?","context":"CI is green.","options":[{"id":"a","label":"Ship","why":"wins a week"},{"id":"b","label":"Wait","why":"gives up the week; worst because nothing is gained"}],"recommended":"a"}"#;

const PARKED_BODY: &str =
    "Some report.\n\n<!-- loom:park Blocked by: #5 by=builder at=2026-11-05T12:00:00Z -->\n";

/// `parity` with every `at=<timestamp>` token masked: `retract` stamps "now",
/// and the two runs can straddle a second.
fn parity_masked(args: &[&str], stdin: Option<&str>, cwd: &Path, home: &Path) -> (i32, String) {
    let mask = |o: &Output| {
        String::from_utf8_lossy(&o.stdout)
            .split_inclusive(char::is_whitespace)
            .map(|w| if w.starts_with("at=") { "at=<T> " } else { w })
            .collect::<String>()
    };
    let go = |bin: &str| {
        let mut c = base(bin, home, cwd);
        c.args(args);
        run(c, stdin)
    };
    let (t, d) = (go(TOOLS), go(DAEMON));
    assert_eq!(t.status.code(), d.status.code(), "exit code differs for {args:?}");
    assert_eq!(mask(&t), mask(&d), "stdout differs for {args:?}");
    (t.status.code().unwrap_or(-1), mask(&t))
}

#[test]
fn park_record_offline_forms_match_loom_daemon_with_no_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, home) = (tmp.path(), tmp.path());

    let render = parity(
        &[
            "park-record",
            "render",
            "--blocked-by",
            "5,#6",
            "--by",
            "doctor",
            "--at",
            "2026-11-05T12:00:00Z",
            "--reason",
            "waits on CI",
        ],
        None,
        cwd,
        home,
    );
    assert_eq!(render.status.code(), Some(0));
    let rendered = String::from_utf8_lossy(&render.stdout).to_string();
    assert!(rendered.contains("Blocked by: #5") && rendered.contains("#6"), "{rendered}");

    // parse from stdin (omitted path and `-`), from a file, plain and --json.
    let found = parity(&["park-record", "parse"], Some(&rendered), cwd, home);
    assert_eq!(found.status.code(), Some(0));
    parity(&["park-record", "parse", "--file", "-", "--json"], Some(&rendered), cwd, home);
    let f = tmp.path().join("body.md");
    std::fs::write(&f, PARKED_BODY).unwrap();
    let file_out =
        parity(&["park-record", "parse", "--file", f.to_str().unwrap()], None, cwd, home);
    assert_eq!(String::from_utf8_lossy(&file_out.stdout).trim(), "5");
    // No record is exit 1 as data.
    let none = parity(&["park-record", "parse"], Some("no record here\n"), cwd, home);
    assert_eq!(none.status.code(), Some(1));

    // retract --file composes offline from stdin or a path; nothing is written.
    let (code, out) = parity_masked(
        &[
            "park-record",
            "retract",
            "--file",
            "-",
            "--blocked-by",
            "5",
            "--reason",
            "not a gate",
        ],
        Some(PARKED_BODY),
        cwd,
        home,
    );
    assert_eq!(code, 0);
    assert!(out.contains("DRY_RUN=true") && out.contains("Park withdrawn:"), "{out}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), PARKED_BODY);
    // A refusal (blank reason) is exit 1 from both, with nothing composed.
    let (code, _) = parity_masked(
        &[
            "park-record",
            "retract",
            "--file",
            f.to_str().unwrap(),
            "--blocked-by",
            "5",
            "--reason",
            " ",
        ],
        None,
        cwd,
        home,
    );
    assert_eq!(code, 1);

    // Journal-only actions: exit 0, no output, no forge.
    for args in [
        &["park-record", "released", "--pr", "7"][..],
        &[
            "park-record",
            "decision",
            "--pr",
            "7",
            "--outcome",
            "pass",
            "--confidence",
            "0.9",
        ],
    ] {
        let o = parity(args, None, cwd, home);
        assert_eq!(o.status.code(), Some(0), "{args:?}");
        assert!(o.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn operator_decision_offline_forms_match_loom_daemon_with_no_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, home) = (tmp.path(), tmp.path());

    let ok = parity(&["operator-decision", "validate", "--input", "-"], Some(DECISION), cwd, home);
    assert_eq!(ok.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("VALID=true"));
    let f = tmp.path().join("d.json");
    std::fs::write(&f, DECISION).unwrap();
    parity(
        &[
            "operator-decision",
            "validate",
            "--input",
            f.to_str().unwrap(),
        ],
        None,
        cwd,
        home,
    );
    // Refusals: contract failure is 1 with reasons on stderr; unreadable is 2.
    let bad = parity(&["operator-decision", "validate", "--input", "-"], Some("{}"), cwd, home);
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("REASON="));
    let missing =
        parity(&["operator-decision", "validate", "--input", "/nonexistent/x"], None, cwd, home);
    assert_eq!(missing.status.code(), Some(2));

    // New-issue preview renders offline: no read, no filing.
    let dry = parity(
        &[
            "operator-decision",
            "apply",
            "--new",
            "--title",
            "T",
            "--input",
            "-",
            "--dry-run",
        ],
        Some(DECISION),
        cwd,
        home,
    );
    assert_eq!(dry.status.code(), Some(0));
    let out = String::from_utf8_lossy(&dry.stdout);
    assert!(
        out.contains("ACTION=create") && out.contains("LABELS_ADD=loom:operator"),
        "{out}"
    );
}

/// The online forms need a forge. With none, both roots fail on the first read
/// with the handler's own forge-failure code (4) and nothing is written; an
/// invalid decision is still refused (1) before any forge call.
#[test]
fn online_forms_without_a_forge_fail_identically_and_write_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, home) = (tmp.path(), tmp.path());
    for (args, stdin, want) in [
        (
            &[
                "park-record",
                "apply",
                "--issue",
                "7",
                "--reason",
                "operator",
                "--repo",
                "acme/widgets",
                "--dry-run",
            ][..],
            None,
            4,
        ),
        (
            &[
                "park-record",
                "retract",
                "--issue",
                "7",
                "--blocked-by",
                "5",
                "--reason",
                "r",
                "--repo",
                "acme/widgets",
            ],
            None,
            4,
        ),
        (
            &[
                "operator-decision",
                "apply",
                "7",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some(DECISION),
            4,
        ),
        (
            &[
                "operator-decision",
                "apply",
                "7",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some("{}"),
            1,
        ),
        (
            &[
                "operator-decision",
                "resolve",
                "7",
                "--option",
                "a",
                "--dry-run",
                "--repo",
                "acme/widgets",
            ],
            None,
            4,
        ),
    ] {
        let o = parity(args, stdin, cwd, home);
        assert_eq!(o.status.code(), Some(want), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(
            !String::from_utf8_lossy(&o.stdout).contains("APPLIED=true"),
            "{args:?} must not report a write"
        );
    }
}

/// A recording of an operator resolution is attended-only: stdin is not a
/// terminal here, so both roots refuse with exit 5 before any forge read, and
/// the tools root does not turn that into a mode error first.
#[test]
fn unattended_resolve_is_refused_with_exit_5_even_with_mode_unset() {
    let tmp = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let args = [
        "operator-decision",
        "resolve",
        "7",
        "--option",
        "a",
        "--repo",
        "acme/widgets",
    ];
    let d = {
        let mut c = base(DAEMON, tmp.path(), tmp.path());
        c.args(args);
        run(c, None)
    };
    let t = {
        let mut c = base(TOOLS, tmp.path(), tmp.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .args(args);
        run(c, None)
    };
    assert_eq!(d.status.code(), Some(5), "{}", String::from_utf8_lossy(&d.stderr));
    assert_eq!(t.status.code(), Some(5), "{}", String::from_utf8_lossy(&t.stderr));
    assert!(String::from_utf8_lossy(&t.stderr).contains("attended"));
}

/// Mode is resolved from the parsed invocation (design §10.3): a form that
/// reaches the forge with `mode` unset and a leaked fleet route is the
/// actionable error before any read or write; an offline form is not gated.
#[test]
fn mode_gate_follows_the_invocation_not_the_verb() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let gated = |args: &[&str], stdin: Option<&str>| {
        let mut c = base(TOOLS, home.path(), wt.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .args(args);
        run(c, stdin)
    };
    for (args, stdin) in [
        (
            vec![
                "park-record",
                "apply",
                "--issue",
                "7",
                "--reason",
                "x",
                "--repo",
                "a/b",
            ],
            None,
        ),
        (
            vec![
                "park-record",
                "retract",
                "--issue",
                "7",
                "--blocked-by",
                "5",
                "--reason",
                "r",
            ],
            None,
        ),
        (vec!["operator-decision", "apply", "7", "--input", "-"], Some(DECISION)),
        (
            vec![
                "operator-decision",
                "apply",
                "--new",
                "--title",
                "T",
                "--input",
                "-",
            ],
            Some(DECISION),
        ),
        (
            vec![
                "operator-decision",
                "resolve",
                "7",
                "--option",
                "a",
                "--dry-run",
            ],
            None,
        ),
    ] {
        let o = gated(&args, stdin);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}");
        assert!(
            err.contains("LOOM_GH_GATEWAY_DIR") && err.contains("LOOM_MODE=session"),
            "{err}"
        );
        assert!(o.stdout.is_empty(), "{args:?}");
    }
    for (args, stdin, want) in [
        (vec!["park-record", "render", "--at", "2026-11-05T12:00:00Z"], None, 0),
        (vec!["park-record", "parse"], Some("x"), 1),
        (vec!["park-record", "released", "--pr", "7"], None, 0),
        (vec!["operator-decision", "validate", "--input", "-"], Some(DECISION), 0),
        (
            vec![
                "operator-decision",
                "apply",
                "--new",
                "--title",
                "T",
                "--input",
                "-",
                "--dry-run",
            ],
            Some(DECISION),
            0,
        ),
    ] {
        let o = gated(&args, stdin);
        assert_eq!(o.status.code(), Some(want), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
}

/// A recording `gh`: appends each argv to `gh.log` and answers an issue read
/// with a parked body, anything else with `{}`. Returns the directory holding
/// the `gh` shim (to put first on `PATH`) and the log path.
fn recording_gh(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh.log");
    let issue = dir.join("issue.json");
    let json = serde_json::json!({
        "body": PARKED_BODY,
        "labels": [{ "name": "loom:building" }],
        "state": "open",
    });
    std::fs::write(&issue, json.to_string()).unwrap();
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$*\" in\n  \
         *issues/7|*pulls/7) cat '{}';;\n  *) echo '{{}}';;\nesac\n",
        log.display(),
        issue.display()
    );
    let gh = bin.join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// An unmanaged target (#9548) is read but never written, on every online form
/// that can mutate, including the nested filing of `apply --new`: the write
/// scope is checked before `create-issue.sh` runs, so a denied target gets no
/// duplicate scan, no filing lock and no forge write. Asserted on the recorded
/// `gh` argv, not on the exit code alone.
#[test]
fn a_denied_target_is_read_but_never_written_on_any_online_form() {
    let tmp = tempfile::tempdir().unwrap();
    let (shim, log) = recording_gh(tmp.path());
    let path = format!("{}:/usr/bin:/bin", shim.display());
    for (args, stdin) in [
        (
            vec![
                "park-record",
                "apply",
                "--issue",
                "7",
                "--reason",
                "operator",
                "--repo",
                "acme/widgets",
                "--remove-label",
                "loom:building",
            ],
            None,
        ),
        (
            vec![
                "park-record",
                "retract",
                "--issue",
                "7",
                "--blocked-by",
                "5",
                "--reason",
                "r",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
        (
            vec![
                "operator-decision",
                "apply",
                "7",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some(DECISION),
        ),
        (
            vec![
                "operator-decision",
                "apply",
                "--new",
                "--title",
                "Ship or wait",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some(DECISION),
        ),
    ] {
        let _ = std::fs::remove_file(&log);
        let mut c = base(TOOLS, tmp.path(), tmp.path());
        c.env("PATH", &path).args(&args);
        let o = run(c, stdin);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(4), "{args:?}: {err}");
        assert!(err.contains("#9548"), "{args:?} must name the write-scope refusal: {err}");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        if !args.contains(&"--new") {
            assert!(
                calls
                    .lines()
                    .any(|call| call == "api repos/acme/widgets/issues/7"),
                "{args:?} did not read the denied issue before the write was refused:\n{calls}"
            );
        }
        for mutation in ["-X", "--input", "issue create", "issue edit"] {
            assert!(
                !calls.contains(mutation),
                "{args:?} issued a forge mutation ({mutation}):\n{calls}"
            );
        }
        assert!(
            !String::from_utf8_lossy(&o.stdout).contains("APPLIED=true"),
            "{args:?} must not report a write"
        );
    }
}

/// A git checkout whose `origin` remote is `origin_url`, carrying a `.loom/`
/// directory, so the write scope answers "managed" for that origin's
/// `owner/repo` (rule 2 of #9548) and the explicit target is the checkout's
/// own origin (rule 1). No commit is needed: remote detection reads
/// `.git/config`.
fn managed_checkout(dir: &Path, origin_url: &str) -> std::path::PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(&repo)
            .output()
            .unwrap()
            .status
            .success(),
        "git init failed"
    );
    assert!(
        Command::new("git")
            .args(["remote", "add", "origin", origin_url])
            .current_dir(&repo)
            .output()
            .unwrap()
            .status
            .success(),
        "git remote add failed"
    );
    repo
}

/// A recording `gh` that answers the write-scope probe as WRITE for
/// `acme/widgets`: the W8 installation listing is unavailable (exit 1), so the
/// per-repo `permissions` read decides, and it names `push`. Issue 7's state
/// is served from `issue_body`; a nested `gh issue create` is answered with a
/// fixed URL. Every successful write echoes `{}`; every argv line lands in the
/// returned log. When `fail_flag` names an existing file, every `-X PATCH`
/// fails (exit 1): the failed-body-write ordering fixture.
fn recording_gh_allowed(
    dir: &Path,
    issue_body: &str,
    fail_flag: Option<&Path>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh.log");
    let issue = dir.join("issue.json");
    let json = serde_json::json!({
        "body": issue_body,
        "labels": [{ "name": "loom:building" }],
        "state": "open",
    });
    std::fs::write(&issue, json.to_string()).unwrap();
    let fail = match fail_flag {
        Some(f) => format!(
            "if [ -f '{}' ] && printf '%s' \"$*\" | grep -q -- '-X PATCH'; then\n\
             echo 'PATCH failed' >&2\n exit 1\nfi\n",
            f.display()
        ),
        None => String::new(),
    };
    // The argv is logged BEFORE the fail check, so a failed write is still
    // recorded — the ordering asserts read that recording.
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\n{fail}case \"$*\" in\n  \
         *--jq*.permissions*) echo '{{\"admin\":true,\"maintain\":true,\"push\":true,\"triage\":true,\"pull\":true}}';;\n  \
         *installation/repositories*) exit 1;;\n  \
         *repos/acme/widgets/issues/7*) cat '{}';;\n  \
         *issue\\ create*) echo 'https://github.com/acme/widgets/issues/12';;\n  \
         *) echo '{{}}';;\nesac\n",
        log.display(),
        issue.display()
    );
    let gh = bin.join("gh");
    std::fs::write(&gh, script).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// Run one binary against the allowed-target fixture: the recorded shim on
/// `PATH`, session mode, and per-run snapshot/verdict cache dirs — both
/// caches otherwise land in host-global directories shared by every test and
/// run, which would make the write-scope probe's gh calls invisible (a warm
/// cache answers without gh). `tag` keeps runs independent.
fn shim_run(
    bin: &str,
    home: &Path,
    cwd: &Path,
    shim: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Output {
    let mut c = base(bin, home, cwd);
    c.env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
        .env("LOOM_LISTING_CACHE_DIR", listing_cache_dir(home, bin))
        .env("LOOM_WRITE_SCOPE_CACHE_DIR", listing_cache_dir(home, bin))
        .args(args);
    run(c, stdin)
}

/// A per-run listing-cache dir under `home`, named by `tag`.
fn listing_cache_dir(home: &Path, tag: &str) -> std::path::PathBuf {
    home.join(format!("listing-cache-{tag}"))
}

/// The composed body a dry-run reports, between its `BODY<<EOF` / `EOF`
/// markers: the handler's own rendering, used to stage a fixture whose next
/// real apply is a no-op.
fn body_from_dry_run(stdout: &str) -> String {
    let start = stdout
        .find("BODY<<EOF\n")
        .expect("dry-run prints BODY<<EOF")
        + "BODY<<EOF\n".len();
    let rest = &stdout[start..];
    // The marker is "\nEOF": its leading newline is the composed body's own
    // final newline, and staging must keep it byte-identical.
    let end = rest.find("\nEOF").expect("dry-run closes EOF");
    rest[..=end].to_string()
}

/// The `issue.json` fixture payload the shim serves for issue 7, from a body.
fn parked_issue(body: &str) -> String {
    serde_json::json!({
        "body": body,
        "labels": [{ "name": "loom:building" }],
        "state": "open",
    })
    .to_string()
}

/// Assert the recorded argv carried a fresh read before any mutation, and the
/// mutations in the contract order (`-X PATCH` body, then label POST, then
/// label DELETE, each optional by marker).
fn assert_write_order(log: &Path, expect_patch: bool, expect_post: bool, expect_delete: bool) {
    let calls = std::fs::read_to_string(log).unwrap();
    let idx = |needle: &str| calls.lines().position(|l| l.contains(needle));
    let view = idx("api repos/acme/widgets/issues/7");
    let patch = idx("-X PATCH");
    let post = idx("-X POST");
    let delete = idx("-X DELETE");
    let first_mutation = [patch, post, delete].into_iter().flatten().min();
    assert!(
        matches!((view, first_mutation), (Some(v), Some(m)) if v < m),
        "the fresh read must come before any mutation:\n{calls}"
    );
    if let (Some(p), Some(q)) = (patch, post) {
        assert!(p < q, "body must be written before labels are applied:\n{calls}");
    }
    if let (Some(q), Some(d)) = (post, delete) {
        assert!(q < d, "label removal must follow the label add:\n{calls}");
    }
    assert_eq!(patch.is_some(), expect_patch, "PATCH presence:\n{calls}");
    assert_eq!(post.is_some(), expect_post, "POST presence:\n{calls}");
    assert_eq!(delete.is_some(), expect_delete, "DELETE presence:\n{calls}");
}

/// An allowed target (all three #9548 rules pass against the fixture) IS
/// written by both binaries, in the contract order: fresh read, body, label
/// add, label remove. `operator-decision apply` and `park-record retract` in
/// the same vein; a repeat that changes nothing writes nothing.
#[test]
fn an_allowed_target_is_written_in_order_by_both_binaries() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@github.com:acme/widgets.git");
    let (shim, log) = recording_gh_allowed(tmp.path(), "hello world", None);
    let park_args = [
        "park-record",
        "apply",
        "--issue",
        "7",
        "--reason",
        "operator",
        "--by",
        "builder",
        "--repo",
        "acme/widgets",
        "--remove-label",
        "loom:building",
    ];
    let mut park_out = None;
    for bin in [TOOLS, DAEMON] {
        let _ = std::fs::remove_file(&log);
        let o = shim_run(bin, tmp.path(), &cwd, &shim, &park_args, None);
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(0), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains("APPLIED=true"), "{bin}: {out}");
        assert!(out.contains("BODY_CHANGED=true"), "{bin}: {out}");
        assert!(out.contains("LABELS_ADD=loom:blocked"), "{bin}: {out}");
        assert!(out.contains("LABELS_REMOVE=loom:building"), "{bin}: {out}");
        assert_write_order(&log, true, true, true);
        if bin == TOOLS {
            park_out = Some(out.to_string());
        }
    }
    // The daemon handler prints the same APPLIED block (parity on the write).
    let mut d = base(DAEMON, tmp.path(), &cwd);
    d.env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
        .args(park_args);
    let d_run = run(d, None);
    let d_out = String::from_utf8_lossy(&d_run.stdout);
    assert_eq!(
        park_out.as_deref().map(str::trim_end),
        Some(d_out.trim_end()),
        "park apply stdout differs"
    );

    // operator-decision apply on the same issue: body first, then the
    // loom:operator label; no removal (nothing to remove).
    let decide = [
        "operator-decision",
        "apply",
        "7",
        "--input",
        "-",
        "--repo",
        "acme/widgets",
    ];
    let decide_dd = [
        "operator-decision",
        "apply",
        "7",
        "--input",
        "-",
        "--repo",
        "acme/widgets",
        "--dry-run",
    ];
    for bin in [TOOLS, DAEMON] {
        let _ = std::fs::remove_file(&log);
        let o = shim_run(bin, tmp.path(), &cwd, &shim, &decide, Some(DECISION));
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(0), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains("APPLIED=true"), "{bin}: {out}");
        assert!(out.contains("LABELS_ADD=loom:operator"), "{bin}: {out}");
        assert_write_order(&log, true, true, false);
    }

    // A repeat that changes nothing writes nothing: stage the issue with the
    // handler's own composed body (from a --dry-run) and the decision label
    // already on, so the next apply recomposes the identical body and finds
    // nothing to add or remove: one read, zero mutations.
    let dry = shim_run(DAEMON, tmp.path(), &cwd, &shim, &decide_dd, Some(DECISION));
    let composed = body_from_dry_run(&String::from_utf8_lossy(&dry.stdout));
    std::fs::write(
        tmp.path().join("issue.json"),
        serde_json::json!({
            "body": composed,
            "labels": [{ "name": "loom:operator" }],
            "state": "open",
        })
        .to_string(),
    )
    .unwrap();
    let _ = std::fs::remove_file(&log);
    let o = shim_run(DAEMON, tmp.path(), &cwd, &shim, &decide, Some(DECISION));
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("APPLIED=true"), "{out}");
    assert!(out.contains("BODY_CHANGED=false"), "{out}");
    assert!(out.contains("LABELS_ADD=\n"), "{out}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("-X"), "a no-op repeat must not mutate:\n{calls}");

    // park-record retract: the body edit only — no label POST or DELETE. A
    // repeat against the already-retracted body (composed offline through the
    // same handler) is exit 0 and writes nothing.
    std::fs::write(tmp.path().join("issue.json"), parked_issue(PARKED_BODY)).unwrap();
    let retract = [
        "park-record",
        "retract",
        "--issue",
        "7",
        "--blocked-by",
        "5",
        "--reason",
        "unblocked",
        "--repo",
        "acme/widgets",
    ];
    for bin in [TOOLS, DAEMON] {
        let _ = std::fs::remove_file(&log);
        let o = shim_run(bin, tmp.path(), &cwd, &shim, &retract, None);
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(0), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains("APPLIED=true"), "{bin}: {out}");
        assert!(out.contains("BODY_CHANGED=true"), "{bin}: {out}");
        assert_write_order(&log, true, false, false);
    }
    let off = shim_run(
        DAEMON,
        tmp.path(),
        &cwd,
        &shim,
        &[
            "park-record",
            "retract",
            "--file",
            "-",
            "--blocked-by",
            "5",
            "--reason",
            "unblocked",
        ],
        Some(PARKED_BODY),
    );
    let retracted = body_from_dry_run(&String::from_utf8_lossy(&off.stdout));
    std::fs::write(tmp.path().join("issue.json"), parked_issue(&retracted)).unwrap();
    let _ = std::fs::remove_file(&log);
    let o = shim_run(DAEMON, tmp.path(), &cwd, &shim, &retract, None);
    let out = String::from_utf8_lossy(&o.stdout);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("BODY_CHANGED=false"), "{out}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("-X"), "an idempotent retract must not mutate:\n{calls}");
}

/// Session mode with a leaked fleet route (`LOOM_GH_GATEWAY_DIR` /
/// `LOOM_GH_READER_ROOT` set, as a parent fleet session exports them) still
/// writes: the mode note names the ignored variables on stderr and the
/// recorded local `gh` — the session's own credential — carries the read and
/// the write. This is the explicit-local-mode routing receipt of #14464.
#[test]
fn session_mode_with_inherited_fleet_variables_writes_through_the_local_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@github.com:acme/widgets.git");
    let (shim, log) = recording_gh_allowed(tmp.path(), "hello world", None);
    let run = |bin: &str, args: &[&str], stdin: Option<&str>| {
        let mut c = base(bin, tmp.path(), &cwd);
        c.env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
            .env("LOOM_LISTING_CACHE_DIR", listing_cache_dir(tmp.path(), bin))
            .env("LOOM_WRITE_SCOPE_CACHE_DIR", listing_cache_dir(tmp.path(), bin))
            .env("LOOM_GH_GATEWAY_DIR", tmp.path().join("gateway"))
            .env("LOOM_GH_READER_ROOT", tmp.path())
            .args(args);
        run(c, stdin)
    };
    for (args, stdin) in [
        (
            vec![
                "park-record",
                "apply",
                "--issue",
                "7",
                "--reason",
                "operator",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
        (
            vec![
                "operator-decision",
                "apply",
                "7",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some(DECISION),
        ),
    ] {
        let _ = std::fs::remove_file(&log);
        let o = run(TOOLS, &args, stdin);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {err}");
        assert!(
            err.contains("ignoring inherited LOOM_GH_GATEWAY_DIR and LOOM_GH_READER_ROOT")
                && err.contains("mode is \"session\" from LOOM_MODE"),
            "{args:?} must ignore the leaked fleet route, not error or adopt it:\n{err}"
        );
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(out.contains("APPLIED=true"), "{args:?}: {out}");
        assert_write_order(&log, true, true, false);
    }
}

/// Fleet issue reads have a gateway profile. Body, label and resolution
/// writes have no corresponding fleet broker operation. An otherwise writable
/// checkout must never send those operations to its ambient gh credential.
#[test]
fn fleet_park_and_decision_writes_stop_before_the_local_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@github.com:acme/widgets.git");
    let (shim, local_log) = recording_gh_allowed(tmp.path(), PARKED_BODY, None);
    let gh = shim.join("gh");
    let gateway = tmp.path().join("gateway/.loom/gh-config-gateway");
    let reader = gateway.join("acme");
    std::fs::create_dir_all(&reader).unwrap();
    std::fs::write(reader.join("hosts.yml"), "github.com: {}\n").unwrap();
    std::fs::write(gateway.join("gh-ok"), gh.to_str().unwrap()).unwrap();
    let state = tmp.path().join("issue.json");
    let initial = std::fs::read(&state).unwrap();

    // Record routed reads separately from every command using the local
    // writer, including failed attempts. The permission fixture grants push.
    let reader_log = tmp.path().join("reader.log");
    let writer_gh = shim.join("local-gh");
    std::fs::rename(&gh, &writer_gh).unwrap();
    std::fs::write(
        &gh,
        r#"#!/bin/sh
set -eu
if [ "${GH_CONFIG_DIR:-}" = "$FLEET_READER" ]; then
    echo "$@" >> "$READER_LOG"
    [ "$*" = 'api repos/acme/widgets/issues/7' ]
    if [ -f "$FLEET_READER/unavailable" ]; then
        echo 'gh: Service Unavailable (HTTP 503)' >&2
        exit 1
    fi
    cat "$ISSUE_STATE"
    exit 0
fi
if [ "${2:-}" = -X ] && [ "${3:-}" = PATCH ]; then
    cp "$6" "$ISSUE_STATE"
fi
exec "$LOCAL_GH" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::metadata(&writer_gh).unwrap().permissions()).unwrap();

    let invoke = |args: &[&str], stdin: Option<&str>| {
        let mut c = base(TOOLS, tmp.path(), &cwd);
        c.env("LOOM_MODE", "fleet")
            .env("LOOM_GH_BIN", &gh)
            .env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
            .env("LOOM_GH_GATEWAY_DIR", &gateway)
            .env("LOCAL_GH", &writer_gh)
            .env("FLEET_READER", &reader)
            .env("READER_LOG", &reader_log)
            .env("ISSUE_STATE", &state)
            .env("LOOM_WRITE_SCOPE_CACHE_DIR", tmp.path().join("scope"))
            .env("LOOM_LISTING_CACHE_DIR", tmp.path().join("listing"))
            .args(args)
            .args(["--repo", "acme/widgets"]);
        run(c, stdin)
    };
    let preview = invoke(
        &[
            "operator-decision",
            "apply",
            "7",
            "--input",
            "-",
            "--dry-run",
        ],
        Some(DECISION),
    );
    assert!(preview.status.success(), "{}", String::from_utf8_lossy(&preview.stderr));
    assert_eq!(
        std::fs::read_to_string(&reader_log).unwrap().trim(),
        "api repos/acme/widgets/issues/7"
    );

    for (args, stdin) in [
        (vec!["park-record", "apply", "--pr", "7", "--reason", "waiting"], None),
        (
            vec![
                "park-record",
                "retract",
                "--issue",
                "7",
                "--blocked-by",
                "5",
                "--reason",
                "landed",
            ],
            None,
        ),
        (vec!["operator-decision", "apply", "7", "--input", "-"], Some(DECISION)),
        (
            vec![
                "operator-decision",
                "apply",
                "--new",
                "--title",
                "Question",
                "--input",
                "-",
            ],
            Some(DECISION),
        ),
    ] {
        let result = invoke(&args, stdin);
        let err = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(4), "{args:?}: {err}");
        assert!(err.contains("scoped fleet write broker"), "{args:?}: {err}");
        assert_eq!(std::fs::read(&state).unwrap(), initial, "{args:?}");
        assert!(
            !local_log.exists(),
            "ambient gh received {:?}",
            std::fs::read_to_string(&local_log)
        );
    }

    // An outage on the configured reader must also stop the workflow.
    std::fs::write(&reader_log, "").unwrap();
    std::fs::write(reader.join("unavailable"), "").unwrap();
    let result = invoke(
        &[
            "park-record",
            "apply",
            "--issue",
            "7",
            "--reason",
            "waiting",
        ],
        None,
    );
    assert_eq!(result.status.code(), Some(4), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(std::fs::read_to_string(&reader_log)
        .unwrap()
        .contains("api repos/acme/widgets/issues/7"));
    assert!(!local_log.exists());
    assert_eq!(std::fs::read(&state).unwrap(), initial);
}

/// `apply --new` files through the workspace's real `create-issue.sh`, which
/// calls `$LOOM_DAEMON_BIN forge create-issue`: on the daemon root that
/// nested create runs and the URL comes back (`CREATED=`, exit 0); on the
/// tools-only root the nested call is the #14396 write-verb refusal (exit 64),
/// the script propagates it, and the handler surfaces exit 4 — after the
/// write scope was vetted, with no forge write of its own. The script's own
/// duplicate (3) and filing-lock (75) refusals pass through both binaries
/// unchanged.
#[test]
fn nested_filing_runs_create_issue_sh_and_surfaces_the_refusal_in_tools_only() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@github.com:acme/widgets.git");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
    let status = Command::new("cp")
        .args(["-r"])
        .arg(&src)
        .arg(cwd.join(".loom").join("scripts"))
        .output()
        .unwrap();
    assert!(status.status.success(), "cp -r defaults/scripts failed");
    let (shim, log) = recording_gh_allowed(tmp.path(), "hello", None);
    let path = format!("{}:/usr/bin:/bin", shim.display());
    let args = [
        "operator-decision",
        "apply",
        "--new",
        "--title",
        "Ship or wait",
        "--input",
        "-",
        "--repo",
        "acme/widgets",
    ];

    // Tools-only root: vetted, then refused at the nested call. No write of
    // its own, no fallback create. This runs FIRST: the probe's snapshot
    // cache (a shared on-disk store) must be cold here, so the vetting is
    // visible as recorded gh calls; the daemon run below may then answer the
    // same probe from that cache.
    let _ = std::fs::remove_file(&log);
    {
        let mut c = base(TOOLS, tmp.path(), &cwd);
        c.env("PATH", &path)
            .env("LOOM_LISTING_CACHE_DIR", listing_cache_dir(tmp.path(), "nested-tools"))
            .env("LOOM_WRITE_SCOPE_CACHE_DIR", listing_cache_dir(tmp.path(), "nested-tools"))
            .args(args);
        let o = run(c, Some(DECISION));
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(4), "{err}");
        assert!(err.contains("not served by loom-tools"), "{err}");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.contains("permissions"),
            "the write scope must be vetted before the nested filing:\n{calls}"
        );
        for mutation in ["-X", "issue create"] {
            assert!(
                !calls.contains(mutation),
                "the tools-only filing issued a forge mutation ({mutation}):\n{calls}"
            );
        }
    }

    // Daemon root: the real script, the real nested create.
    {
        let mut c = base(DAEMON, tmp.path(), &cwd);
        c.env("PATH", &path)
            .env("LOOM_DAEMON_BIN", DAEMON)
            .env("LOOM_LISTING_CACHE_DIR", listing_cache_dir(tmp.path(), "nested-daemon"))
            .env("LOOM_WRITE_SCOPE_CACHE_DIR", listing_cache_dir(tmp.path(), "nested-daemon"))
            .args(args);
        let o = run(c, Some(DECISION));
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains("CREATED=https://github.com/acme/widgets/issues/12"), "{out}");
        assert!(out.contains("LABELS_ADD=loom:operator"), "{out}");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("issue create"), "the nested create must reach gh:\n{calls}");
    }

    // The script's duplicate (3) and filing-lock (75) refusals pass through
    // unchanged on both roots: the handler maps only other failures to 4.
    let script = cwd.join(".loom").join("scripts").join("create-issue.sh");
    for (code, want) in [(3, "duplicate"), (75, "deferred")] {
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho 'create-issue.sh: {want}' >&2\nexit {code}\n"),
        )
        .unwrap();
        for bin in [TOOLS, DAEMON] {
            let mut c = base(bin, tmp.path(), &cwd);
            c.env("PATH", &path)
                .env(
                    "LOOM_LISTING_CACHE_DIR",
                    listing_cache_dir(tmp.path(), &format!("nested-{code}")),
                )
                .env(
                    "LOOM_WRITE_SCOPE_CACHE_DIR",
                    listing_cache_dir(tmp.path(), &format!("nested-{code}")),
                )
                .args(args);
            let o = run(c, Some(DECISION));
            let err = String::from_utf8_lossy(&o.stderr);
            assert_eq!(o.status.code(), Some(code), "{bin}: {err}");
            assert!(err.contains(want), "{bin}: {err}");
        }
    }
}

/// A Gitea checkout is read freely but every write is refused fail-closed:
/// with no resolvable Gitea connection the probe answers Unknown and #9548
/// turns that into a refusal — the existing adapter/decline contract, served
/// identically from both binaries.
#[test]
fn a_gitea_origin_is_read_but_never_written_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@gitea.example.com:acme/widgets.git");
    std::fs::write(cwd.join(".loom").join("config.json"), "{\"forge\":{\"type\":\"gitea\"}}")
        .unwrap();
    let (shim, log) = recording_gh_allowed(tmp.path(), PARKED_BODY, None);
    for (args, stdin) in [
        (
            vec![
                "park-record",
                "apply",
                "--issue",
                "7",
                "--reason",
                "operator",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
        (
            vec![
                "operator-decision",
                "apply",
                "7",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some(DECISION),
        ),
        (
            vec![
                "park-record",
                "retract",
                "--issue",
                "7",
                "--blocked-by",
                "5",
                "--reason",
                "r",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
    ] {
        for bin in [TOOLS, DAEMON] {
            let _ = std::fs::remove_file(&log);
            let o = shim_run(bin, tmp.path(), &cwd, &shim, &args, stdin);
            let err = String::from_utf8_lossy(&o.stderr);
            assert_eq!(o.status.code(), Some(4), "{bin} {args:?}: {err}");
            assert!(err.contains("#9548"), "{bin} {args:?} must name the refusal: {err}");
            assert!(err.contains("Gitea"), "{bin} {args:?}: {err}");
            assert!(
                !String::from_utf8_lossy(&o.stdout).contains("APPLIED=true"),
                "{bin} {args:?} must not report a write"
            );
            let calls = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(
                calls.contains("api repos/acme/widgets/issues/7"),
                "{bin} {args:?} did not read the issue before the write was refused:\n{calls}"
            );
            assert!(!calls.contains("-X"), "{bin} {args:?} issued a forge mutation:\n{calls}");
        }
    }
}

/// The contract ordering under failure: a body write that fails leaves no
/// label behind — no `loom:blocked` POST, no `loom:operator` POST, no
/// retract to report — on both binaries (`park-record apply` and
/// `operator-decision apply` exit 4; stdout reports no write).
#[test]
fn a_failed_body_write_adds_no_label() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@github.com:acme/widgets.git");
    let fail_flag = tmp.path().join("fail-patch");
    std::fs::write(&fail_flag, "").unwrap();
    let (shim, log) = recording_gh_allowed(tmp.path(), "hello world", Some(&fail_flag));
    for (args, stdin, want_msg) in [
        (
            vec![
                "park-record",
                "apply",
                "--issue",
                "7",
                "--reason",
                "operator",
                "--repo",
                "acme/widgets",
            ],
            None,
            "no label applied",
        ),
        (
            vec![
                "operator-decision",
                "apply",
                "7",
                "--input",
                "-",
                "--repo",
                "acme/widgets",
            ],
            Some(DECISION),
            "body write to #7 failed, no label applied",
        ),
    ] {
        for bin in [TOOLS, DAEMON] {
            let _ = std::fs::remove_file(&log);
            let o = shim_run(bin, tmp.path(), &cwd, &shim, &args, stdin);
            let err = String::from_utf8_lossy(&o.stderr);
            assert_eq!(o.status.code(), Some(4), "{bin} {args:?}: {err}");
            assert!(err.contains(want_msg), "{bin} {args:?}: {err}");
            assert!(
                !String::from_utf8_lossy(&o.stdout).contains("APPLIED=true"),
                "{bin} {args:?} must not report a write"
            );
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(
                calls.contains("-X PATCH"),
                "{bin} {args:?} never attempted the body write:\n{calls}"
            );
            assert!(
                !calls.contains("-X POST") && !calls.contains("-X DELETE"),
                "{bin} {args:?} labeled a failed body write:\n{calls}"
            );
        }
    }
}

/// `capabilities --json` lists exactly the nine served subcommands.
#[test]
fn capabilities_list_park_record_and_operator_decision() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = base(TOOLS, tmp.path(), tmp.path());
    c.args(["capabilities", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&run(c, None).stdout).unwrap();
    let cmds: Vec<&str> = v["tools_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    for want in [
        "park-record render",
        "park-record parse",
        "park-record apply",
        "park-record released",
        "park-record decision",
        "park-record retract",
        "operator-decision validate",
        "operator-decision apply",
        "operator-decision resolve",
    ] {
        assert!(cmds.contains(&want), "{want} missing from {cmds:?}");
    }
}

#[path = "loom_tools_slice/pr_lease.rs"]
mod pr_lease;

// ---- #14429: top-level reads and config inspection -------------------------
//
// `base()` gives a clean install: no `loom-daemon` on PATH, no fleet routing,
// no forge credential. Every command runs through the same handler
// `loom-daemon` runs, so `parity` (identical stdout and exit code) is the
// contract; the handlers were read before migrating and none of the served
// forms writes (#14396 holds the writes).

#[test]
fn top_level_reads_match_loom_daemon_with_no_daemon_or_forge() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();

    // `labels`: the registry is embedded, so every read form is deterministic
    // and offline; `check` in a dir with no labels.yml exits 2 on both sides.
    parity(&["labels", "list"], None, cwd, cwd);
    parity(&["labels", "list", "--property", "skip"], None, cwd, cwd);
    let unknown = parity(&["labels", "list", "--property", "bogus"], None, cwd, cwd);
    assert_eq!(unknown.status.code(), Some(2));
    let got = parity(&["labels", "get", "loom:issue"], None, cwd, cwd);
    assert_eq!(got.status.code(), Some(0));
    let missed = parity(&["labels", "get", "no-such-label"], None, cwd, cwd);
    assert_eq!(missed.status.code(), Some(1));
    parity(&["labels", "aliases", "loom:issue"], None, cwd, cwd);
    parity(&["labels", "generate"], None, cwd, cwd);
    let unchecked = parity(&["labels", "check"], None, cwd, cwd);
    assert_eq!(unchecked.status.code(), Some(2));

    // `check-points-marker`: stdin in, exit code is the verdict.
    let ok = parity(
        &["check-points-marker", "--issue", "42"],
        Some("x\n<!-- loom:points=3 -->\n"),
        cwd,
        cwd,
    );
    assert_eq!(ok.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("is pointed 3"));
    let blocked =
        parity(&["check-points-marker", "--issue", "42"], Some("no marker here\n"), cwd, cwd);
    assert_eq!(blocked.status.code(), Some(1));

    // `label-duplicates`: one file in, the count on stdout.
    let labels = cwd.join("labels.yml");
    std::fs::write(&labels, "- name: a\n- name: b\n- name: a\n").unwrap();
    let dup = parity(&["label-duplicates", labels.to_str().unwrap()], None, cwd, cwd);
    assert_eq!(dup.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&dup.stdout).trim(), "1");

    // `duplicate-scan`: the pool is stdin; no forge call.
    parity(
        &[
            "duplicate-scan",
            "--pool",
            "open-issues",
            "--title",
            "Fix the widget",
        ],
        Some("[]"),
        cwd,
        cwd,
    );

    // `affected-files`: not a git checkout -> "could not run" (3), not a verdict.
    let body = cwd.join("body.md");
    std::fs::write(&body, "A report.\n").unwrap();
    let could_not = parity(
        &[
            "affected-files",
            "check",
            "--body-file",
            body.to_str().unwrap(),
        ],
        None,
        cwd,
        cwd,
    );
    assert_eq!(could_not.status.code(), Some(3));

    // `pr-queue`: the offline REST pull array needs no forge.
    let prs = cwd.join("prs.json");
    std::fs::write(&prs, "[]").unwrap();
    parity(
        &[
            "pr-queue",
            "--role",
            "judge",
            "--input",
            prs.to_str().unwrap(),
        ],
        None,
        cwd,
        cwd,
    );

    // `premise-check`: hermetic mode is forge-free; out of scope is exit 0.
    let out = parity(
        &[
            "premise-check",
            "--body-file",
            body.to_str().unwrap(),
            "--title",
            "T",
        ],
        None,
        cwd,
        cwd,
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("SCOPE=out"));
}

/// Commands whose read path reaches the forge fail identically with no
/// credential (the daemon's route is shed by the clean env, so both roots
/// speak to the same absent `gh`), and `check-stale-blocked` keeps its
/// always-exit-0 advisory contract.
#[test]
fn forge_reaching_reads_fail_identically_without_a_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();

    // `check-stale-blocked` reports unevaluated and exits 0 — it never
    // guesses, and never relabels.
    let stale = parity(&["check-stale-blocked", "--json"], None, cwd, cwd);
    assert_eq!(stale.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&stale.stdout).contains("unevaluated"));

    // `pr-queue` without `--input` fetches with the person's own `gh`.
    parity(&["pr-queue", "--role", "judge"], None, cwd, cwd);
    // `premise-check --issue` is forge mode.
    parity(&["premise-check", "--issue", "7"], None, cwd, cwd);
    // `labels undeclared` / `migrate`'s dry runs read the repo's labels.
    parity(
        &[
            "labels",
            "undeclared",
            "--repo",
            "acme/widgets",
            "--labels-file",
            "missing.yml",
        ],
        None,
        cwd,
        cwd,
    );
    parity(&["labels", "migrate", "--repo", "acme/widgets", "--json"], None, cwd, cwd);
    // `forge-probe` without config or credential is "could not run" (2).
    let probe = parity(&["forge-probe"], None, cwd, cwd);
    assert_eq!(probe.status.code(), Some(2));
}

#[test]
fn config_inspection_matches_loom_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();

    // `skip-labels`: the four renderings, offline.
    parity(&["skip-labels"], None, cwd, cwd);
    parity(&["skip-labels", "--json"], None, cwd, cwd);
    parity(&["skip-labels", "--jq-not"], None, cwd, cwd);
    parity(&["skip-labels", "--search"], None, cwd, cwd);

    // `hooks-home`: same HOME, so both roots unpack the same payload and
    // print the same directory.
    let home = parity(&["hooks-home"], None, cwd, cwd);
    assert_eq!(home.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&home.stdout).contains("hooks-home"));

    // `guards`: the status report (a diagnostic, always exit 0) and the
    // remote-exec classifier agree byte for byte.
    parity(&["guards", "status", "--json"], None, cwd, cwd);
    parity(
        &[
            "guards",
            "remote-exec",
            "--for-command",
            "ssh deploy-host rm -rf /srv/app",
        ],
        None,
        cwd,
        cwd,
    );

    // `roles draw`: config resolution over the same workspace root.
    parity(&["roles", "draw", "--json"], None, cwd, cwd);

    // `install-compat show`: the same contract object from both binaries.
    parity(&["install-compat", "show", "--json"], None, cwd, cwd);

    // `forge-inventory`: the embedded manifest, no forge call.
    parity(&["forge-inventory", "validate"], None, cwd, cwd);
    parity(&["forge-inventory", "report"], None, cwd, cwd);
    parity(&["forge-inventory", "probe-manifest", "--profile", "delivery"], None, cwd, cwd);
}

/// A write form is refused before any handler runs, with a status no read
/// form uses as data (the writes are #14396). `loom-daemon` still serves
/// them, so this runs the tools binary only.
#[test]
fn write_forms_of_the_new_commands_are_refused_with_exit_64() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        &["labels", "generate", "--write"][..],
        &[
            "labels",
            "undeclared",
            "--repo",
            "acme/widgets",
            "--labels-file",
            "labels.yml",
            "--prune",
        ],
        &["labels", "migrate", "--apply"],
        &[
            "forge-probe",
            "--live-write",
            "--origin",
            "https://gitea.example.com",
        ],
        &["forge-inventory", "gate", "--update"],
    ] {
        let mut c = base(TOOLS, tmp.path(), tmp.path());
        c.args(args);
        let o = run(c, None);
        assert_eq!(o.status.code(), Some(64), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(o.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("#14396"), "{args:?}: {err}");
    }
}

#[test]
fn label_writes_are_refused_before_mode_resolution() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let gh = tmp.path().join("gh");
    let marker = tmp.path().join("gh-called");
    std::fs::write(&gh, "#!/bin/sh\n: > gh-called\nexit 97\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();

    for (mode, read_error) in [
        (None, "LOOM_GH_GATEWAY_DIR"),
        (Some("invalid-test-mode"), "invalid mode"),
    ] {
        for (args, write_flag) in [
            (
                &[
                    "labels",
                    "undeclared",
                    "--repo",
                    "acme/widgets",
                    "--labels-file",
                    "f",
                ][..],
                "--prune",
            ),
            (&["labels", "migrate"][..], "--apply"),
        ] {
            let command = || {
                let mut c = base(TOOLS, tmp.path(), tmp.path());
                c.env_remove("LOOM_MODE")
                    .env("LOOM_GH_GATEWAY_DIR", tmp.path())
                    .env("LOOM_GH_BIN", &gh)
                    .args(args);
                if let Some(mode) = mode {
                    c.env("LOOM_MODE", mode);
                }
                c
            };
            let mut write = command();
            write.arg(write_flag);
            let refused = run(write, None);
            let err = String::from_utf8_lossy(&refused.stderr);
            assert_eq!(refused.status.code(), Some(64), "{mode:?} {args:?} {write_flag}: {err}");
            assert!(refused.stdout.is_empty() && err.contains("#14396"), "{err}");

            let read = run(command(), None);
            let err = String::from_utf8_lossy(&read.stderr);
            assert_eq!(read.status.code(), Some(1), "{mode:?} {args:?}: {err}");
            assert!(read.stdout.is_empty() && err.contains(read_error), "{err}");
        }
    }
    assert!(!marker.exists(), "mode validation or refusal reached the fake forge");
}

/// Design §7.3 / §10.3: a forge-reaching form resolves `mode` first; an
/// inherited gateway variable with `mode` unset is the actionable error, and
/// an offline form with the same environment is not gated at all.
#[test]
fn forge_reaching_new_commands_resolve_mode_first() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let gated = |args: &[&str], stdin: Option<&str>| {
        let mut c = base(TOOLS, home.path(), wt.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .args(args);
        run(c, stdin)
    };
    for args in [
        &["check-stale-blocked", "--json"][..],
        &["pr-queue", "--role", "judge"],
        &["premise-check", "--issue", "7"],
        &[
            "labels",
            "undeclared",
            "--repo",
            "acme/widgets",
            "--labels-file",
            "f",
        ],
        &["labels", "migrate"],
        &["forge-probe", "--origin", "https://gitea.example.com"],
    ] {
        let o = gated(args, None);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("LOOM_GH_GATEWAY_DIR") && err.contains("LOOM_MODE=session"),
            "{args:?}: {err}"
        );
        assert!(o.stdout.is_empty(), "{args:?}");
    }
    // Offline forms with the same environment just run.
    std::fs::write(wt.path().join("body.md"), "A report.\n").unwrap();
    std::fs::write(wt.path().join("prs.json"), "[]").unwrap();
    for (args, stdin, want) in [
        (&["labels", "list"][..], None, 0),
        (&["skip-labels", "--json"], None, 0),
        (&["duplicate-scan", "--pool", "open-issues", "--title", "t"], Some("[]"), 0),
        (&["premise-check", "--body-file", "body.md", "--title", "T"], None, 0),
        (&["pr-queue", "--role", "judge", "--input", "prs.json"], None, 0),
    ] {
        let o = gated(args, stdin);
        assert_eq!(o.status.code(), Some(want), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
}

/// `capabilities --json` lists the new families, and `loom-daemon
/// install-compat show --json` prints the same list.
#[test]
fn capabilities_list_the_new_reads_and_config_inspection() {
    let tmp = tempfile::tempdir().unwrap();
    let t = run(
        {
            let mut c = base(TOOLS, tmp.path(), tmp.path());
            c.args(["capabilities", "--json"]);
            c
        },
        None,
    );
    assert_eq!(t.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&t.stdout).unwrap();
    let cmds: Vec<&str> = v["tools_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    for want in [
        "labels",
        "pr-queue",
        "premise-check",
        "duplicate-scan",
        "check-points-marker",
        "label-duplicates",
        "forge-probe",
        "forge-inventory",
        "affected-files",
        "check-stale-blocked",
        "skip-labels",
        "roles",
        "guards",
        "install-compat",
        "hooks-home",
    ] {
        assert!(cmds.contains(&want), "{want} missing from {cmds:?}");
    }
    let d = run(
        {
            let mut c = base(DAEMON, tmp.path(), tmp.path());
            c.args(["install-compat", "show", "--json"]);
            c
        },
        None,
    );
    let dv: serde_json::Value = serde_json::from_slice(&d.stdout).unwrap();
    assert_eq!(dv["tools_commands"], v["tools_commands"]);
}

// ---------------------------------------------------------------------------
// `claim` family (#14487): the checkout-local claim commands, served by the
// same `script_helpers::claim::run` handler from both binaries.
// ---------------------------------------------------------------------------

/// A checkout root (`.git` + `.loom`) with a nested directory to run from, so
/// every test also proves root discovery from a subdirectory.
struct Checkout {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    sub: std::path::PathBuf,
}

impl Checkout {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join(".loom")).unwrap();
        let sub = root.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        Self {
            _tmp: tmp,
            root,
            sub,
        }
    }

    fn lock(&self, issue: u32) -> std::path::PathBuf {
        self.root
            .join(".loom/claims")
            .join(format!("issue-{issue}.lock"))
    }

    fn claim_json(&self, issue: u32) -> String {
        std::fs::read_to_string(self.lock(issue).join("claim.json")).unwrap()
    }

    /// Run `claim <args>` with `bin` from the checkout's subdirectory, in a
    /// clean environment with `mode` (when given) and no daemon anywhere.
    fn claim(&self, bin: &str, mode: Option<&str>, args: &[&str]) -> Output {
        let mut c = Command::new(bin);
        c.current_dir(&self.sub)
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", "/usr/bin:/bin")
            .arg("claim")
            .args(args);
        if let Some(m) = mode {
            c.env("LOOM_MODE", m);
        }
        run(c, None)
    }
}

/// `mode` values the family must behave identically under, `None` being unset.
const CLAIM_MODES: [Option<&str>; 4] = [None, Some("session"), Some("local-daemon"), Some("fleet")];

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Replace every `YYYY-MM-DDTHH:MM:SSZ` timestamp so output from two
/// checkouts compares equal.
fn mask_timestamps(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let is_ts = i + 20 <= b.len()
            && b[i..i + 20].iter().enumerate().all(|(k, c)| match k {
                4 | 7 => *c == b'-',
                10 => *c == b'T',
                13 | 16 => *c == b':',
                19 => *c == b'Z',
                _ => c.is_ascii_digit(),
            });
        if is_ts {
            out.push_str("<ts>");
            i += 20;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// All six verbs, the competing owner and every exit code, from a
/// subdirectory, with no daemon, under each `mode` (including unset).
#[test]
fn claim_family_works_offline_in_every_mode_with_the_winner_intact() {
    for mode in CLAIM_MODES {
        let co = Checkout::new();
        let t = |args: &[&str]| co.claim(TOOLS, mode, args);

        // Acquire as A (0): the lock lands under the checkout root.
        assert_eq!(code(&t(&["claim", "42", "agent-a", "3600"])), 0, "{mode:?}");
        assert!(co.lock(42).is_dir(), "{mode:?}: lock under the checkout root");
        assert!(!co.sub.join(".loom").exists(), "{mode:?}: nothing under the cwd");
        let saved = co.claim_json(42);
        let v: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(v["agent_id"], "agent-a");
        assert_eq!(v["ttl_seconds"], 3600);

        // check (0) and list show the claim.
        let check = t(&["check", "42"]);
        assert_eq!(code(&check), 0, "{mode:?}");
        assert!(stdout(&check).contains("agent-a"), "{mode:?}");
        let list = t(&["list"]);
        assert_eq!(code(&list), 0, "{mode:?}");
        assert!(
            stdout(&list).contains("Issue #42 - Agent: agent-a"),
            "{mode:?}: {}",
            stdout(&list)
        );

        // A competing owner B: existing claim (1), mismatch (4) on extend and
        // release. Each refusal leaves the winner's record byte-identical.
        assert_eq!(code(&t(&["claim", "42", "agent-b", "60"])), 1, "{mode:?}");
        assert_eq!(co.claim_json(42), saved, "{mode:?}: after competing claim");
        assert_eq!(code(&t(&["extend", "42", "agent-b", "9999"])), 4, "{mode:?}");
        assert_eq!(co.claim_json(42), saved, "{mode:?}: after foreign extend");
        assert_eq!(code(&t(&["release", "42", "agent-b"])), 4, "{mode:?}");
        assert!(co.lock(42).is_dir(), "{mode:?}: foreign release kept the lock");
        assert_eq!(co.claim_json(42), saved, "{mode:?}: after foreign release");

        // Extend as A with a distinct TTL: owner kept, TTL and expiry updated.
        assert_eq!(code(&t(&["extend", "42", "agent-a", "7200"])), 0, "{mode:?}");
        let v2: serde_json::Value = serde_json::from_str(&co.claim_json(42)).unwrap();
        assert_eq!(v2["agent_id"], "agent-a", "{mode:?}");
        assert_eq!(v2["ttl_seconds"], 7200, "{mode:?}");
        assert_eq!(v2["claimed_at"], v["claimed_at"], "{mode:?}");
        assert!(v2["expires_at"].as_str() > v["expires_at"].as_str(), "{mode:?}");

        // Release as A (0): the lock directory is gone; check then reports 3.
        assert_eq!(code(&t(&["release", "42", "agent-a"])), 0, "{mode:?}");
        assert!(!co.lock(42).exists(), "{mode:?}: released");
        assert_eq!(code(&t(&["check", "42"])), 3, "{mode:?}");
        assert_eq!(code(&t(&["release", "42", "agent-a"])), 3, "{mode:?}");
        assert_eq!(code(&t(&["extend", "42", "agent-a"])), 3, "{mode:?}");

        // Bad input (2).
        assert_eq!(code(&t(&["claim", "not-a-number"])), 2, "{mode:?}");
        assert_eq!(code(&t(&["check"])), 2, "{mode:?}");
        assert_eq!(code(&t(&["extend", "42"])), 2, "{mode:?}");
        assert_eq!(code(&t(&["bogus"])), 2, "{mode:?}");
    }
}

/// Cleanup removes only the expired claim and leaves the live one untouched.
#[test]
fn claim_cleanup_removes_only_the_expired_claim() {
    for mode in CLAIM_MODES {
        let co = Checkout::new();
        assert_eq!(code(&co.claim(TOOLS, mode, &["claim", "7", "live-agent", "3600"])), 0);
        let live = co.claim_json(7);
        // An expired claim, seeded directly in the on-disk format.
        std::fs::create_dir_all(co.lock(8)).unwrap();
        std::fs::write(
            co.lock(8).join("claim.json"),
            r#"{"issue":8,"agent_id":"ghost","claimed_at":"2020-01-01T00:00:00Z","expires_at":"2020-01-01T00:30:00Z","ttl_seconds":1800}"#,
        )
        .unwrap();

        assert_eq!(code(&co.claim(TOOLS, mode, &["cleanup"])), 0, "{mode:?}");
        assert!(!co.lock(8).exists(), "{mode:?}: expired claim removed");
        assert!(co.lock(7).is_dir(), "{mode:?}: live claim kept");
        assert_eq!(co.claim_json(7), live, "{mode:?}: live record untouched");
    }
}

/// `loom-tools claim` and `loom-daemon claim` agree on exit code and stdout
/// for the same sequence, each run in its own equivalent checkout so one
/// binary's mutations cannot affect the other's result.
#[test]
fn claim_family_matches_loom_daemon_on_separate_checkouts() {
    for mode in CLAIM_MODES {
        let (ct, cd) = (Checkout::new(), Checkout::new());
        for args in [
            vec!["claim", "5", "agent-a", "3600"],
            vec!["claim", "5", "agent-b"],
            vec!["check", "5"],
            vec!["list"],
            vec!["extend", "5", "agent-b"],
            vec!["extend", "5", "agent-a", "100"],
            vec!["release", "5", "agent-b"],
            vec!["release", "5", "agent-a"],
            vec!["check", "5"],
            vec!["release", "5"],
            vec!["claim", "x"],
            vec!["cleanup"],
            vec!["list"],
            vec!["help"],
            vec![],
        ] {
            let (t, d) = (ct.claim(TOOLS, mode, &args), cd.claim(DAEMON, mode, &args));
            assert_eq!(code(&t), code(&d), "{mode:?} {args:?}: exit code differs");
            assert_eq!(
                mask_timestamps(&stdout(&t)),
                mask_timestamps(&stdout(&d)),
                "{mode:?} {args:?}: stdout differs"
            );
        }
    }
}

/// Outside a Loom checkout the family exits 2, as the daemon does.
#[test]
fn claim_outside_a_checkout_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = base(TOOLS, tmp.path(), tmp.path());
    c.args(["claim", "check", "1"]);
    assert_eq!(code(&run(c, None)), 2);
}

/// `capabilities --json` lists the claim family.
#[test]
fn capabilities_list_the_claim_family() {
    let tmp = tempfile::tempdir().unwrap();
    let mut c = base(TOOLS, tmp.path(), tmp.path());
    c.args(["capabilities", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&run(c, None).stdout).unwrap();
    assert!(
        v["tools_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "claim"),
        "claim missing from {v}"
    );
}
