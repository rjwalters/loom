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

/// #14519: the shared write verbs are served. Without a usable forge
/// credential (`gh` with no auth config) both roots fail identically on the
/// one shared handler and write nothing; the exit code is the handler's, not
/// the refusal's. Each case gets a fresh home so a held verdict lock (an
/// expected local success) cannot leak into the next case.
#[test]
fn shared_write_verbs_are_served_and_fail_identically_without_a_forge() {
    for (args, stdin) in [
        (
            &[
                "forge",
                "comment",
                "7",
                "--body",
                "x",
                "--repo",
                "acme/widgets",
            ][..],
            None,
        ),
        (
            &[
                "forge",
                "comment",
                "7",
                "--repo",
                "acme/widgets",
                "--body-file",
                "-",
            ],
            Some("body from stdin\n"),
        ),
        (
            &[
                "forge",
                "create-issue",
                "--title",
                "T",
                "--body",
                "B",
                "--repo",
                "acme/widgets",
                "--json",
            ],
            None,
        ),
        (&["forge", "rerun", "12345", "--repo", "acme/widgets"], None),
        (
            &[
                "forge",
                "verdict-labels",
                "7",
                "--repo",
                "acme/widgets",
                "--verdict",
                "approved",
            ],
            None,
        ),
        (
            &[
                "forge",
                "verdict-reconcile",
                "7",
                "--repo",
                "acme/widgets",
                "--verdict",
                "approved",
                "--sha",
                "a",
            ],
            None,
        ),
        (
            // Idempotent and forge-free: the acquire/contention/release
            // lifecycle itself is covered offline below.
            &[
                "forge",
                "verdict-lock",
                "release",
                "7",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
        (
            &[
                "forge",
                "doctor-handback",
                "7",
                "--expected-head-sha",
                "a",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
        (&["forge", "auto-merge", "7"], None),
        (&["forge", "disable-auto-merge", "7"], None),
        (
            &[
                "forge",
                "parent",
                "link",
                "--parent",
                "1",
                "--child",
                "2",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
        (
            // `child_number` vetoes a cross-repo child before any forge call:
            // nothing is written, identically from both roots.
            &[
                "forge",
                "parent",
                "link",
                "--parent",
                "1",
                "--child",
                "other/repo#2",
                "--repo",
                "acme/widgets",
            ],
            None,
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let o = parity(args, stdin, tmp.path(), tmp.path());
        assert_ne!(
            o.status.code(),
            Some(64),
            "{args:?} must be served, not refused: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&o.stderr).contains("#14520"),
            "{args:?} must not hit the merge-queue refusal"
        );
    }
}

#[test]
fn forge_operator_holds_is_daemon_only() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let roster = root.join("fleet.json");
    let gh = root.join("gh");
    let log = root.join("forge-reads.log");
    let fixture = serde_json::json!({
        "_generated": { "schema_version": 1 },
        "root": root,
        "config": { "defaults": {} },
        "state": {},
        "repos": [{
            "name": "widgets",
            "remote": "https://github.com/acme/widgets",
            "fleet": true,
            "weight": 1
        }]
    });
    std::fs::write(&roster, fixture.to_string()).unwrap();
    std::fs::write(
        &gh,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$READ_LOG"
case "$*" in
    'api '*repos/acme/widgets/pulls\?*)
        printf 'HTTP/2.0 200 OK\r\nx-ratelimit-remaining: 4000\r\n\r\n[]\n'
        ;;
    *) exit 98 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let invoke = |bin| {
        let mut cmd = base(bin, root, root);
        cmd.env("LOOM_GH_BIN", &gh)
            .env("LOOM_LISTING_CACHE_DIR", root.join("listing"))
            .env("READ_LOG", &log)
            .args(["forge", "operator-holds", "--fleet-json"])
            .arg(&roster);
        run(cmd, None)
    };
    let refused = invoke(TOOLS);
    let err = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(
        refused.status.code(),
        Some(loom_daemon::cli::EX_WRITE_VERB_NOT_MIGRATED),
        "{err}"
    );
    assert!(err.contains("use `loom-daemon forge operator-holds`"), "{err}");
    assert!(refused.stdout.is_empty());
    assert!(!log.exists(), "loom-tools must refuse before reaching the forge");

    let allowed = invoke(DAEMON);
    assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stderr));
    let output: serde_json::Value = serde_json::from_slice(&allowed.stdout).unwrap();
    assert_eq!(output["complete"], true);
    assert_eq!(output["repos_examined"], 1);
    assert_eq!(output["repos"][0]["repo"], "acme/widgets");
    let reads = std::fs::read_to_string(log).unwrap();
    assert_eq!(reads.lines().count(), 1, "{reads}");
    assert!(reads.contains("repos/acme/widgets/pulls?"), "{reads}");
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
/// verbs served by #14395, #14519 and #14520. `install-compat show --json`
/// prints the same list.
#[test]
fn capabilities_list_the_served_forge_verbs() {
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
    for command in [
        "forge check-claim",
        "forge wait-checks",
        "forge merge-queue status",
        "forge merge-queue enqueue",
        "forge merge-queue dequeue",
        "forge merge-queue reconcile",
        "forge merge-queue handoff",
        "forge merge-queue step",
        "forge merge-queue revoke",
        "forge merge-queue group-check",
        "forge merge-queue authorize-check",
        "forge issue",
    ] {
        assert!(cmds.contains(&command), "{command} missing from {cmds:?}");
    }
    // #14519: the shared non-queue writes are served.
    for write in [
        "forge comment",
        "forge create-issue",
        "forge rerun",
        "forge auto-merge",
        "forge disable-auto-merge",
        "forge verdict-labels",
        "forge verdict-reconcile",
        "forge verdict-lock",
        "forge doctor-handback",
        "forge journal-ci-refusal",
        "forge parent link",
    ] {
        assert!(cmds.contains(&write), "{write} missing from {cmds:?}");
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

// ---- #14519: the shared non-queue writes ----------------------------------

/// A served verb that can reach the forge is gated exactly like a read: with
/// `mode` unset and an inherited gateway variable, the actionable error comes
/// before any write — stdout stays empty, including the `--json` error object
/// `create-issue` would print for a forge failure.
#[test]
fn gated_write_verbs_stop_before_the_write_with_mode_unset() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    for args in [
        &["forge", "comment", "7", "--body", "x"][..],
        &[
            "forge",
            "create-issue",
            "--title",
            "T",
            "--body",
            "B",
            "--json",
        ],
        &["forge", "rerun", "1"],
        &[
            "forge",
            "verdict-labels",
            "7",
            "--repo",
            "acme/widgets",
            "--verdict",
            "approved",
        ],
        &["forge", "doctor-handback", "7", "--expected-head-sha", "a"],
    ] {
        let mut c = base(TOOLS, home.path(), wt.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .args(args);
        let o = run(c, None);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("LOOM_GH_GATEWAY_DIR") && err.contains("LOOM_MODE=session"),
            "{args:?}: {err}"
        );
        assert!(o.stdout.is_empty(), "{args:?} wrote before the gate");
    }
}

/// #14519: the local write verbs (`verdict-lock`, `journal-ci-refusal`)
/// touch local state only, so they run with `mode` unset and inherited fleet
/// variables present — no gate, no route, and the daemon reads the same
/// handler (plus its own lock/journal wiring), so nothing changes there.
#[test]
fn local_write_verbs_run_offline_with_mode_unset_and_fleet_variables() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let locks = tempfile::tempdir().unwrap();
    let journal = home.path().join("pick.jsonl");
    let offline = |argv: &[&str], journal_env: Option<&Path>| {
        let mut c = base(TOOLS, home.path(), wt.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .env("LOOM_VERDICT_LOCK_DIR", locks.path());
        match journal_env {
            Some(j) => c.env("LOOM_PICK_JOURNAL", j),
            None => c.env_remove("LOOM_PICK_JOURNAL"),
        };
        c.args(argv);
        run(c, None)
    };

    // `verdict-lock`: acquire takes a per-PR directory; a rival acquire with
    // a zero wait is refused with exit 9 (fail closed); release removes it.
    let lock = locks.path().join("acme_widgets-7");
    let o = offline(
        &[
            "forge",
            "verdict-lock",
            "acquire",
            "7",
            "--repo",
            "acme/widgets",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(lock.is_dir(), "{lock:?} must exist after acquire");
    let rival = {
        let mut c = base(TOOLS, home.path(), wt.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .env("LOOM_VERDICT_LOCK_DIR", locks.path())
            .env("LOOM_VERDICT_LOCK_WAIT_SECS", "0")
            .args([
                "forge",
                "verdict-lock",
                "acquire",
                "7",
                "--repo",
                "acme/widgets",
            ]);
        run(c, None)
    };
    assert_eq!(rival.status.code(), Some(9), "{}", String::from_utf8_lossy(&rival.stderr));
    let o = offline(
        &[
            "forge",
            "verdict-lock",
            "release",
            "7",
            "--repo",
            "acme/widgets",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(0));
    assert!(!lock.exists(), "{lock:?} must be gone after release");

    // `journal-ci-refusal`: a recognized class appends one marker line; an
    // unknown class and an unset journal are no-ops. Exit 0 in every case.
    let o = offline(
        &[
            "forge",
            "journal-ci-refusal",
            "7",
            "--class",
            "ci_red",
            "--head",
            "abc",
        ],
        Some(journal.as_path()),
    );
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let line = std::fs::read_to_string(&journal).unwrap();
    assert!(line.contains("judge:ci-refused") && line.contains("ci_red"), "{line}");

    let unknown = home.path().join("pick2.jsonl");
    let o = offline(
        &["forge", "journal-ci-refusal", "7", "--class", "ci_green"],
        Some(unknown.as_path()),
    );
    assert_eq!(o.status.code(), Some(0));
    assert!(!unknown.exists(), "an unknown class must not journal");

    let o = offline(&["forge", "journal-ci-refusal", "7", "--class", "ci_red"], None);
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
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

/// A recording `gh` for the unverifiable case (#14621): the installation
/// listing fails (exit 1, so the W8 snapshot answers Unavailable) and every
/// other call is answered with `{}` — the per-repo probe reads the all-false
/// `none` role an App installation token is served everywhere, which is no
/// verdict, so the credential cannot be verified and the caller must fail
/// closed. Every argv lands in the returned log.
fn unverifiable_gh(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("gh.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$*\" in\n  \
         *installation/repositories*) echo 'gh: listing unavailable' >&2; exit 1;;\n  \
         *) echo '{{}}';;\nesac\n",
        log.display()
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
///
/// #14621 extends the table with the #14519 write verbs (`forge comment`,
/// `verdict-labels`, `verdict-reconcile`, `doctor-handback`, `rerun`,
/// `create-issue` from the tools root, and `auto-merge`/`disable-auto-merge`).
/// Their vet runs before any forge call (`read` is false), and the refusal
/// exit is each entry point's own: the central forge
/// gate and `create-issue` exit 1, `rerun` exits 3 with its sentinel on
/// stdout — only `park-record`/`operator-decision` exit 4. The no-`--repo`
/// auto-merge verbs run from a remoteless directory, so their target is
/// unresolved: rule 1's NoTarget refusal (a rule-2 "unmanaged origin" denial
/// is not producible for them — with an empty workspace registry
/// `effective_roots` falls back to the checkout itself, which makes any
/// checkout whose origin matches the resolved target managed; their
/// credential-unverifiable refusal is the sibling test below).
#[test]
fn a_denied_target_is_read_but_never_written_on_any_online_form() {
    let tmp = tempfile::tempdir().unwrap();
    let (shim, log) = recording_gh(tmp.path());
    let path = format!("{}:/usr/bin:/bin", shim.display());
    for (args, stdin, want, read, cwd) in [
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
            4,
            true,
            tmp.path(),
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
            4,
            true,
            tmp.path(),
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
            4,
            true,
            tmp.path(),
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
            4,
            false,
            tmp.path(),
        ),
        // #14621: the #14519 write verbs, from the tools root, against a
        // denied explicit target.
        (
            vec![
                "forge",
                "comment",
                "7",
                "--body",
                "x",
                "--repo",
                "acme/widgets",
            ],
            None,
            1,
            false,
            tmp.path(),
        ),
        (
            vec![
                "forge",
                "verdict-labels",
                "7",
                "--repo",
                "acme/widgets",
                "--verdict",
                "approved",
            ],
            None,
            1,
            false,
            tmp.path(),
        ),
        (
            vec![
                "forge",
                "verdict-reconcile",
                "7",
                "--repo",
                "acme/widgets",
                "--verdict",
                "approved",
                "--sha",
                "a",
            ],
            None,
            1,
            false,
            tmp.path(),
        ),
        (
            vec![
                "forge",
                "doctor-handback",
                "7",
                "--expected-head-sha",
                "a",
                "--repo",
                "acme/widgets",
            ],
            None,
            1,
            false,
            tmp.path(),
        ),
        (
            vec!["forge", "rerun", "12345", "--repo", "acme/widgets"],
            None,
            3,
            false,
            tmp.path(),
        ),
        (
            vec![
                "forge",
                "create-issue",
                "--title",
                "T",
                "--body",
                "B",
                "--repo",
                "acme/widgets",
            ],
            None,
            1,
            false,
            tmp.path(),
        ),
        // No `--repo` and no git remote: the target cannot be resolved, so
        // rule 1's NoTarget refusal fires before any handler or probe.
        (vec!["forge", "auto-merge", "7"], None, 1, false, tmp.path()),
        (vec!["forge", "disable-auto-merge", "7"], None, 1, false, tmp.path()),
    ] {
        std::fs::write(&log, "").unwrap();
        let mut c = base(TOOLS, tmp.path(), cwd);
        c.env("PATH", &path).args(&args);
        let o = run(c, stdin);
        let err = String::from_utf8_lossy(&o.stderr);
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(want), "{args:?}: {err}{out}");
        let rerun = args.starts_with(&["forge", "rerun"]);
        if rerun {
            assert!(
                out.split_once('\n').is_some_and(|(line, rest)| {
                    line.starts_with("LOOM-RERUN-ERROR ")
                        && line.contains("#9548")
                        && rest.is_empty()
                }),
                "{args:?} must print exactly one write-scope rerun error: {out:?}"
            );
        } else {
            assert!(
                err.contains("#9548"),
                "{args:?} must name the write-scope refusal on stderr: {err}"
            );
        }
        let calls = std::fs::read_to_string(&log).unwrap();
        if args[0] == "forge" {
            assert!(calls.is_empty(), "{args:?} made a forge call before the refusal:\n{calls}");
            if !rerun {
                assert!(o.stdout.is_empty(), "{args:?} must refuse with empty stdout: {out:?}");
            }
            continue;
        }
        if read {
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
        assert!(!out.contains("APPLIED=true"), "{args:?} must not report a write: {out}");
    }
}

/// A *managed* target whose credential cannot be verified is still refused,
/// with the probe's own reads the only `gh` traffic (rule 2 passes, rule 3
/// fails closed, #14621). The installation listing is unavailable and the
/// per-repo role is the all-false `none` an App token is served, so the
/// verdict is Unverifiable — distinct from the unmanaged refusals of the
/// table above. Covered for one `write_target` verb (`comment`), one
/// in-handler verb (`create-issue`), and the no-`--repo` auto-merge pair
/// (their target resolves to the checkout's managed origin; this is the only
/// producible refusal for them beyond NoTarget, see the table above), from
/// the tools root. Each case gets a fresh home and cache dirs (via
/// `shim_run`) so the probe's one-minute Unknown cache, the snapshot failure
/// backoff, and the host-global write-scope cache cannot answer a case
/// without `gh`.
#[test]
fn an_unverifiable_managed_target_is_refused_without_a_write() {
    for args in [
        &[
            "forge",
            "comment",
            "7",
            "--body",
            "x",
            "--repo",
            "acme/widgets",
        ][..],
        &[
            "forge",
            "create-issue",
            "--title",
            "T",
            "--body",
            "B",
            "--repo",
            "acme/widgets",
        ][..],
        &["forge", "auto-merge", "7"][..],
        &["forge", "disable-auto-merge", "7"][..],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (shim, log) = unverifiable_gh(tmp.path());
        let checkout = managed_checkout(tmp.path(), "https://github.com/acme/widgets.git");
        let o = shim_run(TOOLS, tmp.path(), &checkout, &shim, args, None);
        let err = String::from_utf8_lossy(&o.stderr);
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {err}{out}");
        assert!(
            err.contains("#9548"),
            "{args:?} must name the write-scope refusal: {err} | {out}"
        );
        assert!(
            err.contains("could not verify"),
            "{args:?} must refuse as unverifiable, not unmanaged: {err}"
        );
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls,
            concat!(
                "api --include installation/repositories?per_page=100&page=1\n",
                "api repos/acme/widgets --jq .permissions // {}\n",
            ),
            "{args:?} must only make the two credential reads"
        );
        assert!(o.stdout.is_empty(), "{args:?} must refuse with empty stdout: {out:?}");
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
    let mut park_outs: Vec<String> = Vec::new();
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
        park_outs.push(out.to_string());
    }
    // The daemon handler prints the same APPLIED block (parity on the write).
    // Both outputs come from the isolated `shim_run` runs above: a third,
    // unisolated daemon run would write host-global cache entries.
    assert_eq!(park_outs.len(), 2, "one park apply per binary");
    assert_eq!(park_outs[0].trim_end(), park_outs[1].trim_end(), "park apply stdout differs");

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

/// Publish a fake fleet gateway the way the daemon does: the base and the
/// owner profile are private directories (0700), `gh-ok` and `hosts.yml` are
/// private files (0600), and the marker names `gh` canonically. The router's
/// trust checks reject group/other-writable paths, so relying on the ambient
/// umask makes the fixture pass under 0022 and shed under 0002. Chmod each
/// path explicitly instead of touching the process-wide umask.
fn publish_fake_gateway(gateway: &Path, owner: &str, gh: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let private = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let profile = gateway.join(owner);
    std::fs::create_dir_all(&profile).unwrap();
    private(gateway, 0o700);
    private(&profile, 0o700);
    let marker = gateway.join("gh-ok");
    std::fs::write(&marker, gh.canonicalize().unwrap().as_os_str().as_encoded_bytes()).unwrap();
    private(&marker, 0o600);
    let hosts = profile.join("hosts.yml");
    std::fs::write(&hosts, "github.com: {}\n").unwrap();
    private(&hosts, 0o600);
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
    publish_fake_gateway(&gateway, "acme", &gh);
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

/// `apply --new` files through the real `create-issue.sh`, which calls
/// `$LOOM_DAEMON_BIN forge create-issue`. Both roots reach the same filing
/// handler and return the created URL. The script's duplicate (3) and
/// filing-lock (75) refusals also pass through both roots.
#[test]
fn nested_filing_runs_create_issue_sh_through_both_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = managed_checkout(tmp.path(), "git@github.com:acme/widgets.git");
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
    let copy = Command::new("cp")
        .args(["-r"])
        .arg(&src)
        .arg(cwd.join(".loom").join("scripts"))
        .output()
        .unwrap();
    assert!(copy.status.success(), "cp -r defaults/scripts failed");
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

    for (bin, tag) in [(TOOLS, "nested-tools"), (DAEMON, "nested-daemon")] {
        let _ = std::fs::remove_file(&log);
        let mut c = base(bin, tmp.path(), &cwd);
        c.env("PATH", &path)
            .env("LOOM_LISTING_CACHE_DIR", listing_cache_dir(tmp.path(), tag))
            .env("LOOM_WRITE_SCOPE_CACHE_DIR", listing_cache_dir(tmp.path(), tag))
            .args(args);
        if bin == DAEMON {
            c.env("LOOM_DAEMON_BIN", DAEMON);
        }
        let o = run(c, Some(DECISION));
        let out = String::from_utf8_lossy(&o.stdout);
        assert_eq!(o.status.code(), Some(0), "{bin}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(
            out.contains("CREATED=https://github.com/acme/widgets/issues/12"),
            "{bin}: {out}"
        );
        assert!(out.contains("LABELS_ADD=loom:operator"), "{bin}: {out}");
        let calls = std::fs::read_to_string(&log).unwrap();
        let scope_probe = calls
            .find("permissions")
            .unwrap_or_else(|| panic!("{bin}: {calls}"));
        let create = calls
            .find("issue create")
            .unwrap_or_else(|| panic!("{bin}: {calls}"));
        assert!(scope_probe < create, "{bin}: scope must be vetted before filing:\n{calls}");
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

/// #14488: `capabilities --json` lists all served issue-lease verbs.
#[test]
fn capabilities_list_the_issue_lease_verbs() {
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
        "lease ensure",
        "lease co-occupancy",
        "lease release",
        "lease renewer",
    ] {
        assert!(cmds.contains(&want), "{want} missing from {cmds:?}");
    }
}

/// #14488: the offline `lease renewer` probe the scripts use to select a binary
/// answers identically from both binaries, with `mode` unset.
#[test]
fn lease_renewer_probe_matches_loom_daemon_with_mode_unset() {
    let tmp = tempfile::tempdir().unwrap();
    for argv in [
        &["lease", "renewer", "--help"][..],
        &["lease", "renewer", "sanitize-exec", "--check"],
    ] {
        let run_one = |bin: &str| {
            let mut c = base(bin, tmp.path(), tmp.path());
            c.env_remove("LOOM_MODE").args(argv);
            run(c, None)
        };
        let (t, d) = (run_one(TOOLS), run_one(DAEMON));
        assert_eq!(t.status.code(), d.status.code(), "{argv:?}");
        assert_eq!(t.status.code(), Some(0), "{argv:?}");
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

// ---- #14520: the merge-queue actions --------------------------------------
//
// Both roots parse the one `ForgeAction` tree and run the one
// `forge_merge_queue::handle`, so the real binaries must agree on stdout and
// exit code. `base()` has no `champion.mergeMode`, so the default (`direct`)
// applies unless a test sets `LOOM_MERGE_MODE`.

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn queue_verbs() -> Vec<Vec<&'static str>> {
    vec![
        vec!["forge", "merge-queue", "reconcile", "--mode-only"],
        vec!["forge", "merge-queue", "reconcile", "--pr", "7"],
        vec!["forge", "merge-queue", "step", "7", "--approved-sha", SHA],
        vec![
            "forge",
            "merge-queue",
            "revoke",
            "7",
            "--reason",
            "stale-verdict",
        ],
        vec![
            "forge",
            "merge-queue",
            "enqueue",
            "7",
            "--approved-sha",
            SHA,
        ],
        vec!["forge", "merge-queue", "dequeue", "7"],
        vec![
            "forge",
            "merge-queue",
            "handoff",
            "7",
            "--approved-sha",
            SHA,
        ],
    ]
}

/// Direct mode (the default): `reconcile` (with or without `--mode-only`) and
/// `step` answer `DIRECT` before any forge call, identically from both
/// binaries, with no credential, no `gh`, and `LOOM_MODE` unset behind a leaked
/// gateway variable, which would be an error for a gated verb.
#[test]
fn merge_queue_direct_mode_short_circuits_match_loom_daemon_without_a_forge() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    for args in queue_verbs()
        .into_iter()
        .filter(|a| matches!(a[2], "reconcile" | "step"))
    {
        let run_one = |bin: &str| {
            let mut c = base(bin, home.path(), wt.path());
            c.env_remove("LOOM_MODE")
                .env("LOOM_GH_GATEWAY_DIR", gw.path())
                .args(&args);
            run(c, None)
        };
        let t = run_one(TOOLS);
        let d = run_one(DAEMON);
        let err = String::from_utf8_lossy(&t.stderr);
        assert!(
            !err.contains("LOOM_GH_GATEWAY_DIR"),
            "{args:?} must not be mode-gated in direct mode: {err}"
        );
        assert_eq!(t.status.code(), Some(0), "{args:?}: {err}");
        assert_eq!(t.status.code(), d.status.code(), "{args:?}: {err}");
        assert_eq!(t.stdout, d.stdout, "{args:?}");
        let out = String::from_utf8_lossy(&t.stdout);
        assert!(out.starts_with("LOOM-MERGE-QUEUE-DIRECT"), "{args:?}: {out}");
    }
}

/// The verbs `write_target` vets, and the queue-mode paths, may reach the
/// forge, so the mode gate runs first; `reconcile --mode-only` reads and
/// writes nothing and is not gated; the two checks are gated in every mode.
#[test]
fn merge_queue_mode_gate_follows_the_resolved_merge_mode() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let gw = tempfile::tempdir().unwrap();
    let gated = |args: &[&str], merge_mode: Option<&str>| {
        let mut c = base(TOOLS, home.path(), wt.path());
        c.env_remove("LOOM_MODE")
            .env("LOOM_GH_GATEWAY_DIR", gw.path())
            .args(args);
        if let Some(m) = merge_mode {
            c.env("LOOM_MERGE_MODE", m);
        }
        run(c, None)
    };
    let assert_gated = |args: &[&str], merge_mode: Option<&str>| {
        let o = gated(args, merge_mode);
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(1), "{args:?} {merge_mode:?}: {err}");
        assert!(
            err.contains("LOOM_GH_GATEWAY_DIR") && err.contains("LOOM_MODE=session"),
            "{args:?} {merge_mode:?}: {err}"
        );
        assert!(o.stdout.is_empty(), "{args:?} {merge_mode:?}");
    };
    for args in queue_verbs()
        .into_iter()
        .filter(|a| !a.contains(&"--mode-only"))
    {
        assert_gated(&args, Some("queue"));
        // Vetted before the handler's direct-mode refusal: gated in every mode.
        if matches!(args[2], "enqueue" | "dequeue" | "handoff" | "revoke") {
            assert_gated(&args, None);
        }
    }
    assert_gated(&["forge", "merge-queue", "group-check", "--commit", SHA], None);
    assert_gated(
        &[
            "forge",
            "merge-queue",
            "authorize-check",
            "7",
            "--pr-head",
            SHA,
        ],
        None,
    );
    // `reconcile --mode-only` is observable with no forge effect and no mode.
    let o = gated(&["forge", "merge-queue", "reconcile", "--mode-only"], Some("queue"));
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        "LOOM-MERGE-QUEUE-MODE queue execution=enabled"
    );
}

/// A `gh` that records every argv and fails every call: any write it were
/// asked to make would show up in the log.
fn failing_gh(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh-calls.log");
    let path = dir.join("gh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\necho 'gh: not authorized' >&2\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Queue mode with a target the writer cannot vet (an unreadable scope): the
/// writing verbs refuse or report "nothing confirmed" and make no remote
/// mutation, and `loom-tools` and `loom-daemon` agree on every answer.
#[test]
fn merge_queue_unreadable_scope_makes_no_remote_mutation() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    failing_gh(bin.path());
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let args_list: Vec<Vec<&str>> = vec![
        vec![
            "forge",
            "merge-queue",
            "reconcile",
            "--pr",
            "7",
            "--repo",
            "acme/widgets",
        ],
        vec![
            "forge",
            "merge-queue",
            "step",
            "7",
            "--approved-sha",
            SHA,
            "--repo",
            "acme/widgets",
        ],
        vec![
            "forge",
            "merge-queue",
            "revoke",
            "7",
            "--repo",
            "acme/widgets",
        ],
        vec![
            "forge",
            "merge-queue",
            "enqueue",
            "7",
            "--approved-sha",
            SHA,
            "--repo",
            "acme/widgets",
        ],
        vec![
            "forge",
            "merge-queue",
            "handoff",
            "7",
            "--approved-sha",
            SHA,
            "--repo",
            "acme/widgets",
        ],
        vec![
            "forge",
            "merge-queue",
            "dequeue",
            "7",
            "--repo",
            "acme/widgets",
        ],
        vec![
            "forge",
            "merge-queue",
            "group-check",
            "--commit",
            SHA,
            "--repo",
            "acme/widgets",
        ],
    ];
    for args in args_list {
        let mut outs = Vec::new();
        for b in [TOOLS, DAEMON] {
            let mut c = base(b, home.path(), wt.path());
            c.env("PATH", &path)
                .env("LOOM_MERGE_MODE", "queue")
                .env("LOOM_GH_BIN", bin.path().join("gh"))
                .args(&args);
            outs.push(run(c, None));
        }
        let (t, d) = (&outs[0], &outs[1]);
        let err = String::from_utf8_lossy(&t.stderr);
        assert_eq!(t.status.code(), d.status.code(), "{args:?}: {err}");
        assert_ne!(t.status.code(), Some(0), "{args:?} must not succeed: {err}");
        let first = String::from_utf8_lossy(&t.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        assert!(
            !first.starts_with("LOOM-MERGE-QUEUE-CONTINUE")
                && !first.starts_with("LOOM-MERGE-QUEUE-DIRECT")
                && !first.starts_with("LOOM-MERGE-QUEUE-HANDED-OFF")
                && !first.starts_with("LOOM-MERGE-QUEUE-MERGED"),
            "{args:?}: {first}"
        );
    }
    let log = std::fs::read_to_string(bin.path().join("gh-calls.log")).unwrap_or_default();
    for write in [
        "POST", "PATCH", "DELETE", "--method", "-X", "mutation", "comment", "edit",
    ] {
        assert!(!log.contains(write), "a remote mutation ({write}) was attempted:\n{log}");
    }
}
