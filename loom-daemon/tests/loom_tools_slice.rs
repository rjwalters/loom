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
