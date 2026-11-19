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

fn worktree_fixture_git(home: &Path, repo: &Path, args: &[&str]) -> Output {
    let mut command = base("git", home, repo);
    command.args(args);
    let output = run(command, None);
    assert!(output.status.success(), "{output:?}");
    output
}

fn init_worktree_fixture(home: &Path, repo: &Path) {
    std::fs::create_dir(repo).unwrap();
    worktree_fixture_git(home, repo, &["init", "-q", "-b", "main"]);
    worktree_fixture_git(
        home,
        repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn worktree_check_runs_offline_with_spaces_and_no_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("primary checkout");
    let worktree = tmp.path().join("linked checkout");
    init_worktree_fixture(tmp.path(), &repo);
    worktree_fixture_git(
        tmp.path(),
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    let args = ["worktree-check", "--porcelain"];
    let tools = run(
        {
            let mut command = base(TOOLS, tmp.path(), &worktree);
            command.env_remove("LOOM_MODE").args(args);
            command
        },
        None,
    );
    let daemon = run(
        {
            let mut command = base(DAEMON, tmp.path(), &worktree);
            command.args(args);
            command
        },
        None,
    );
    assert!(daemon.status.success(), "{daemon:?}");
    assert_eq!(tools.status.code(), daemon.status.code(), "{tools:?}");
    assert_eq!(tools.stdout, daemon.stdout);
    assert_eq!(tools.stderr, daemon.stderr);
    assert!(String::from_utf8_lossy(&tools.stdout).contains("primary checkout"));
}

#[cfg(unix)]
#[test]
fn worktree_closed_branch_reads_follow_tools_mode() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("branch fixture");
    init_worktree_fixture(tmp.path(), &repo);
    let git = |args: &[&str]| worktree_fixture_git(tmp.path(), &repo, args);
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/fixture.git",
    ]);
    git(&["update-ref", "refs/remotes/origin/feature/example", "HEAD"]);
    let head = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();
    let payload = serde_json::json!([{"headRefOid":head.trim(), "number":19, "state":"CLOSED", "mergedAt":null}]);

    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    let log = tmp.path().join("read.log");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
test "$1 $2" = 'pr list' || exit 90
printf '%s\n' "${GH_CONFIG_DIR:-session}" >> "$FIXTURE_LOG"
if test "$FIXTURE_FAIL" = 1; then exit 1; fi
printf '%s\n' "$FIXTURE_RESPONSE"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let gateway = tmp.path().join(".loom/gh-config-gateway");
    let profile = gateway.join("acme");
    std::fs::create_dir_all(&profile).unwrap();
    for path in [&gateway, &profile] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let hosts = profile.join("hosts.yml");
    std::fs::write(&hosts, "github.com: {}\n").unwrap();
    std::fs::set_permissions(&hosts, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(gateway.join("gh-ok"), gh.canonicalize().unwrap().to_str().unwrap()).unwrap();

    let decision = |binary, mode, fail, branch| {
        let mut command = base(binary, tmp.path(), &repo);
        command
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("LOOM_MODE", mode)
            .env("FIXTURE_LOG", &log)
            .env("FIXTURE_FAIL", fail)
            .env("FIXTURE_RESPONSE", payload.to_string());
        if binary == TOOLS {
            command.env("LOOM_GH_GATEWAY_DIR", &gateway);
        }
        command
            .args([
                "worktree-closed-pr-branch",
                "--branch",
                branch,
                "--issue",
                "42",
                "--base-display",
                "main",
                "--json-output",
                "true",
            ])
            .arg("--repo-root")
            .arg(&repo);
        run(command, None)
    };
    let legacy = decision(DAEMON, "invalid", "0", "feature/example");
    assert_eq!(legacy.status.code(), Some(1), "{legacy:?}");
    for mode in ["session", "fleet"] {
        let tools = decision(TOOLS, mode, "0", "feature/example");
        assert_eq!(tools.status.code(), Some(1), "{tools:?}");
        assert_eq!(tools.stdout, legacy.stdout);
    }
    let failure = decision(TOOLS, "fleet", "1", "feature/example");
    assert_eq!(failure.status.code(), Some(0), "{failure:?}");
    assert!(failure.stdout.is_empty());
    let invalid = decision(TOOLS, "invalid", "0", "feature/example");
    assert_invalid_worktree_mode(&invalid);
    let absent_tip = decision(TOOLS, "invalid", "0", "feature/absent");
    assert!(absent_tip.status.success(), "{absent_tip:?}");
    assert!(absent_tip.stdout.is_empty(), "{absent_tip:?}");
    assert_eq!(
        std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "session",
            "session",
            profile.to_str().unwrap(),
            profile.to_str().unwrap()
        ]
    );

    let open_pr = |binary, mode| {
        let mut command = base(binary, tmp.path(), &repo);
        command
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("LOOM_MODE", mode)
            .env("FIXTURE_LOG", &log)
            .env("FIXTURE_FAIL", "1")
            .args(["worktree-open-pr", "--branch", "feature/example", "--repo"])
            .arg(&repo);
        run(command, None)
    };
    for (binary, mode) in [(TOOLS, "session"), (DAEMON, "invalid")] {
        let failure = open_pr(binary, mode);
        assert!(failure.status.success(), "{failure:?}");
        assert_eq!(failure.stdout, b"STATUS\tunavailable\n");
    }
    let reads_before = std::fs::read(&log).unwrap();
    assert_invalid_worktree_mode(&open_pr(TOOLS, "invalid"));
    assert_eq!(std::fs::read(log).unwrap(), reads_before);
}

#[cfg(unix)]
fn assert_invalid_worktree_mode(output: &Output) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid"), "{stderr}");
    assert!(stderr.contains("LOOM_MODE"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn worktree_existing_uses_the_primary_mode_and_the_linked_git_context() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let primary = home.join("primary checkout");
    let linked = primary.join(".loom/worktrees/issue-42");
    let origin = home.join("bare origin");
    let bin = home.join("bin");
    let log = home.join("forge-cwd.log");
    let branch = "feature/issue-42";
    init_worktree_fixture(home, &primary);
    let git = |args: &[&str]| worktree_fixture_git(home, &primary, args);
    git(&["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        branch,
        linked.to_str().unwrap(),
    ]);
    std::fs::write(linked.join("work.txt"), "unique work\n").unwrap();
    worktree_fixture_git(home, &linked, &["add", "work.txt"]);
    worktree_fixture_git(
        home,
        &linked,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "unique work",
        ],
    );
    git(&["push", "-q", "origin", "main", branch]);
    git(&[
        "branch",
        "--set-upstream-to",
        "origin/feature/issue-42",
        branch,
    ]);
    std::fs::write(primary.join(".git/info/exclude"), ".loom-managed\n.loom/\n").unwrap();
    let head = String::from_utf8(git(&["rev-parse", branch]).stdout).unwrap();
    let payload = serde_json::json!([{"headRefOid": head.trim(), "number": 19}]).to_string();

    std::fs::create_dir(primary.join(".loom-local")).unwrap();
    std::fs::write(primary.join(".loom-local/local.json"), r#"{"mode":"session"}"#).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        r#"#!/bin/sh
test "$1 $2 $3 $4 $5 $6" = 'pr list --head feature/issue-42 --state merged' || exit 90
pwd -P >> "$FIXTURE_LOG"
printf '%s\n' "$FIXTURE_RESPONSE"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let decision = |binary, mode: Option<&str>| {
        let mut command = base(binary, home, &primary);
        command.env_remove("LOOM_MODE");
        if let Some(mode) = mode {
            command.env("LOOM_MODE", mode);
        }
        command
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("FIXTURE_LOG", &log)
            .env("FIXTURE_RESPONSE", &payload)
            .args([
                "worktree-existing",
                "--issue",
                "42",
                "--branch",
                branch,
                "--default-branch",
                "main",
                "--base-ref",
                "origin/main",
                "--base-display",
                "origin/main",
                "--repo",
            ])
            .arg(&primary)
            .arg("--worktree")
            .arg(&linked);
        run(command, None)
    };

    let invalid = decision(TOOLS, Some("invalid"));
    assert_invalid_worktree_mode(&invalid);
    assert!(!log.exists(), "{invalid:?}");
    assert!(!linked.join(".loom-managed").exists(), "{invalid:?}");
    assert_eq!(git(&["rev-parse", branch]).stdout, head.as_bytes());

    let tools = decision(TOOLS, None);
    assert!(tools.status.success(), "{tools:?}");
    assert!(
        String::from_utf8_lossy(&tools.stdout).contains("preserving existing work"),
        "{tools:?}"
    );
    assert!(linked.join(".loom-managed").is_file());
    assert_eq!(git(&["rev-parse", branch]).stdout, head.as_bytes());
    let legacy = decision(DAEMON, Some("invalid"));
    assert!(legacy.status.success(), "{legacy:?}");
    assert_eq!(tools.stdout, legacy.stdout);
    let linked = linked.canonicalize().unwrap();
    let reads = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        reads.lines().collect::<Vec<_>>(),
        [linked.to_str().unwrap(), linked.to_str().unwrap()]
    );

    let mut removal = base(TOOLS, home, &primary);
    removal
        .env("LOOM_MODE", "invalid")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("FIXTURE_LOG", &log)
        .args(["worktree-remove", "42", "--json"]);
    let removal = run(removal, None);
    assert_eq!(removal.status.code(), Some(1), "{removal:?}");
    assert!(String::from_utf8_lossy(&removal.stderr).contains("invalid mode"), "{removal:?}");
    let report: serde_json::Value = serde_json::from_slice(&removal.stdout).unwrap();
    assert_eq!(report["success"], false);
    assert_eq!(report["removed"], true);
    assert_eq!(report["branchStatus"], "kept");
    assert!(!linked.exists());
    assert_eq!(git(&["rev-parse", branch]).stdout, head.as_bytes());
    assert_eq!(std::fs::read_to_string(log).unwrap(), reads);
}

#[test]
fn worktree_capabilities_match_both_binaries_and_the_migrated_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let capabilities = |binary, args: &[&str]| {
        let mut command = base(binary, tmp.path(), tmp.path());
        command.args(args);
        let output = run(command, None);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["tools_commands"]
            .clone()
    };
    let tools = capabilities(TOOLS, &["capabilities", "--json"]);
    let daemon = capabilities(DAEMON, &["install-compat", "show", "--json"]);
    assert_eq!(tools, daemon);
    let mut worktree_commands: Vec<_> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|name| name.starts_with("worktree-"))
        .collect();
    worktree_commands.sort_unstable();
    assert_eq!(
        worktree_commands,
        [
            "worktree-base",
            "worktree-branch-conflict",
            "worktree-branch-reuse",
            "worktree-check",
            "worktree-cleanup",
            "worktree-closed-pr-branch",
            "worktree-existing",
            "worktree-link",
            "worktree-lock",
            "worktree-open-pr",
            "worktree-remove",
            "worktree-sparse",
            "worktree-submodules",
        ]
    );
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
