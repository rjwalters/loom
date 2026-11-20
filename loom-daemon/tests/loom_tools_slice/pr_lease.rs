use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::{base, run, TOOLS};

#[test]
fn configured_gitea_pr_leases_stay_label_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("workspace");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join(".loom")).unwrap();
    fs::write(root.join(".loom/config.json"), r#"{"forge":{"type":"gitea"}}"#).unwrap();
    let gh = tmp.path().join("gh");
    let log = tmp.path().join("calls");
    fs::write(&gh, "#!/bin/sh\necho called >> \"$RECORDER\"\nexit 1\n").unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();

    for action in ["claim", "check", "fence", "release"] {
        let mut command = base(TOOLS, tmp.path(), tmp.path());
        command
            .env_remove("LOOM_MODE")
            .env("LOOM_GH_BIN", &gh)
            .env("RECORDER", &log)
            .args([
                "lease",
                "pr",
                action,
                "77",
                "--role",
                "judge",
                "--workspace",
            ])
            .arg(&root);
        let out = run(command, None);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{action}: {stderr}");
        assert!(stderr.contains("on Gitea the label claim stands alone"), "{stderr}");
        assert!(out.stdout.is_empty(), "{action}");
    }
    assert!(!log.exists());
}

use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use tempfile::TempDir;

use super::DAEMON;

struct Fixture {
    tmp: TempDir,
    root: PathBuf,
    launcher: PathBuf,
    gh: PathBuf,
    gateway: PathBuf,
}

impl Fixture {
    fn new(mode: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        let root = path.join("workspace");
        let launcher = path.join("launcher");
        let gh = path.join("gh");
        let gateway = path.join("gateway/.loom/gh-config-gateway");

        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::create_dir(&launcher).unwrap();
        fs::create_dir(path.join("writer")).unwrap();
        for args in [
            ["init", "--quiet", "", ""],
            ["remote", "add", "origin", "https://github.com/acme/widgets"],
        ] {
            let mut git = base("/usr/bin/git", path, &root);
            git.args(args.into_iter().filter(|arg| !arg.is_empty()));
            assert!(run(git, None).status.success());
        }
        // Config belongs to the selected checkout. The launcher deliberately
        // disagrees, so accidentally resolving cwd fails even for local mode.
        fs::write(
            root.join(".loom/config.json"),
            json!({"mode": mode, "forge": {"type": "github"}}).to_string(),
        )
        .unwrap();
        fs::create_dir(launcher.join(".loom")).unwrap();
        fs::write(launcher.join(".loom/config.json"), r#"{"mode":"invalid"}"#).unwrap();

        fs::write(&gh, include_str!("pr_lease_forge.sh")).unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).unwrap();
        super::publish_fake_gateway(&gateway, "acme", &gh);
        fs::write(path.join("rows"), "").unwrap();
        fs::write(path.join("next"), "500\n").unwrap();
        Self {
            tmp,
            root,
            launcher,
            gh,
            gateway,
        }
    }

    fn command(&self, binary: &str, verb: &str) -> Command {
        let path = self.tmp.path();
        let mut command = base(binary, path, &self.launcher);
        command
            .env_remove("LOOM_MODE")
            .env("LOOM_GH_BIN", &self.gh)
            .env("LOOM_GH_GATEWAY_DIR", &self.gateway)
            .env("GH_CONFIG_DIR", path.join("writer"))
            .env("LOOM_HOST_ID", "host-00000012")
            .env("LOOM_LEASE_PUBLISH_HOSTNAME", "1")
            .env("LOOM_LEASE_RENEW_STATE_DIR", path.join("ownership"))
            .env("SWEEP_LEASE_RENEW_INTERVAL_SECS", "1")
            .env("LOOM_TEST_PR_LEASE", path)
            .args(["lease", "pr", verb, "77", "--workspace"])
            .arg(&self.root);
        command
    }

    fn lease(&self, binary: &str, verb: &str, role: &str, sweep: &str, extra: &[&str]) -> Output {
        let mut command = self.command(binary, verb);
        command
            .args(["--role", role, "--sweep-id", sweep])
            .args(extra);
        run(command, None)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.tmp.path().join(name)).unwrap_or_default()
    }

    fn seed(&self, id: u64, role: &str, sweep: &str, minutes: i64) -> String {
        use std::io::Write as _;
        let body = format!("<!-- loom:pr-lease role={role} host=host-00000012 sweep={sweep} -->");
        let row = json!({
            "id": id,
            "updated_at": (Utc::now() - chrono::Duration::minutes(minutes))
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            "body": body,
            "user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"},
            "author_association": "NONE",
        });
        let mut rows = fs::OpenOptions::new()
            .append(true)
            .open(self.tmp.path().join("rows"))
            .unwrap();
        writeln!(rows, "{row}").unwrap();
        body
    }

    fn renewer_log(&self) -> String {
        fs::read_to_string(self.root.join(".loom/logs/pr-lease/pr-77-judge.log"))
            .unwrap_or_default()
    }

    fn owners(&self) -> Vec<Value> {
        fs::read_dir(self.tmp.path().join("ownership"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "owner"))
            .filter_map(|entry| serde_json::from_slice(&fs::read(entry.path()).ok()?).ok())
            .collect()
    }

    fn assert_routes(&self, reader: &str) {
        let calls = self.read("calls");
        assert!(
            calls
                .lines()
                .any(|line| line == format!("{reader} comments")),
            "{calls}"
        );
        assert!(calls.lines().any(|line| line == format!("{reader} state")), "{calls}");
        for line in calls.lines() {
            let (actual, operation) = line.split_once(' ').unwrap();
            let expected = match operation {
                "permissions" | "post" | "renew" | "release" | "delete" => "writer",
                "comments" | "state" | "head" | "checks" => reader,
                _ => panic!("unexpected recorded call: {line}"),
            };
            assert_eq!(actual, expected, "{line}\n{calls}");
        }
    }
}

struct Watcher(Child);

impl Watcher {
    fn new() -> Self {
        Self(Command::new("/bin/sleep").arg("30").spawn().unwrap())
    }

    fn stop(&mut self) {
        let _ = self.0.kill();
        self.0.wait().unwrap();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.stop();
        }
    }
}

fn assert_code(out: &Output, code: i32) -> &str {
    let stderr = std::str::from_utf8(&out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(code), "{stderr}");
    stderr
}

fn eventually(expectation: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(Instant::now() < deadline, "{expectation}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn selected_workspace_routes_real_renewal_and_release_in_all_modes() {
    for (mode, reader) in [
        ("fleet", "gateway"),
        ("session", "writer"),
        ("local-daemon", "writer"),
    ] {
        let f = Fixture::new(mode);
        let other_role = f.seed(100, "curator", "other", 0);
        let mut watch = Watcher::new();
        let watch_id = watch.0.id().to_string();
        let claim = |sweep| {
            f.lease(TOOLS, "claim", "judge", sweep, &["--watch-pid", &watch_id, "--step", "judge"])
        };

        let first = claim("attended");
        let stderr = assert_code(&first, 0);
        assert!(stderr.contains("lease 500 held"), "{stderr}");
        if mode != "fleet" {
            assert!(stderr.contains("ignoring inherited LOOM_GH_GATEWAY_DIR"), "{stderr}");
        }
        assert_eq!(first.stdout, b"host-00000012 attended\n");
        eventually("detached child records a renewal PATCH", || {
            f.read("calls").contains("writer renew\n")
        });
        let owners = f.owners();
        assert_eq!(owners.len(), 1, "{owners:?}");
        let owner = &owners[0];
        let pid = owner["pid"].as_u64().unwrap();
        assert!(pid > 1 && pid != u64::from(watch.0.id()));
        assert_eq!(owner["repo"], "acme/widgets");
        assert_eq!(owner["host"], "host-00000012");
        assert_eq!(owner["sweep"], "attended#pr-judge");
        assert_eq!(owner["issue"], 77);
        let ident = owner["ident"].as_str().unwrap();
        assert!(ident.starts_with("lstart:") || ident.starts_with("starttime:"), "{ident}");
        let process = Command::new("/bin/ps")
            .args(["-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let argv = String::from_utf8_lossy(&process.stdout);
        assert!(argv.contains("loom-tools lease pr renew-loop"), "{argv}");
        assert!(argv.contains(&format!("--watch-pid {watch_id}")), "{argv}");
        assert!(argv.contains("--step=judge"), "{argv}");
        assert!(f
            .renewer_log()
            .contains(&format!("renewer {pid} started for comment 500 (pid {watch_id})")));

        let repeat = claim("attended");
        assert!(assert_code(&repeat, 0).contains("comment 500"));
        eventually("re-attach does not replace the owner", || {
            f.renewer_log().contains("already owns this lease; exiting")
        });
        assert_eq!(f.owners(), owners);
        assert_eq!(
            f.read("calls")
                .lines()
                .filter(|line| *line == "writer post")
                .count(),
            1
        );
        assert_code(&claim("peer"), 4);
        let check = f.lease(TOOLS, "check", "judge", "attended", &[]);
        assert_code(&check, 0);
        assert!(check.stdout.starts_with(b"PR_LEASE_STATE=held-self"));
        let peer = f.lease(TOOLS, "check", "judge", "peer", &[]);
        assert_code(&peer, 4);
        assert!(peer.stdout.starts_with(b"PR_LEASE_STATE=held-peer"));
        assert_code(&f.lease(TOOLS, "fence", "judge", "attended", &[]), 0);
        assert_code(&f.lease(TOOLS, "fence", "judge", "peer", &[]), 4);
        assert_code(&f.lease(TOOLS, "claim", "doctor", "another", &[]), 7);
        assert_code(&f.lease(TOOLS, "release", "judge", "mismatch", &[]), 8);

        watch.stop();
        eventually("renewer stops when the watched process exits", || {
            f.renewer_log().contains("the watched session ended")
        });
        assert_code(
            &f.lease(
                TOOLS,
                "release",
                "judge",
                "attended",
                &["--skip-reason", "gate-refused:lease"],
            ),
            0,
        );
        assert_code(&f.lease(TOOLS, "release", "judge", "attended", &[]), 0);
        let rows = f.read("rows");
        assert!(rows.contains("loom:pr-lease-released role=judge"), "{rows}");
        assert!(rows.contains(&other_role), "{rows}");
        assert_eq!(
            f.read("calls")
                .lines()
                .filter(|line| *line == "writer release")
                .count(),
            1
        );
        assert_code(&claim("attended"), 6);
        f.assert_routes(reader);
    }
}

#[test]
fn unreadable_expired_and_unconfigured_routes_keep_their_contracts() {
    let f = Fixture::new("fleet");
    f.seed(100, "judge", "expired", 60);
    assert_code(&f.lease(TOOLS, "fence", "judge", "expired", &[]), 3);
    fs::write(f.tmp.path().join("fail-comments"), "").unwrap();

    assert_code(&f.lease(TOOLS, "claim", "judge", "me", &[]), 5);
    let check = f.lease(TOOLS, "check", "judge", "me", &[]);
    assert_code(&check, 5);
    assert!(check.stdout.starts_with(b"PR_LEASE_STATE=unknown"));
    assert_code(&f.lease(TOOLS, "fence", "judge", "me", &[]), 0);
    assert!(!f.read("calls").contains("writer comments"));
    assert!(!f.read("calls").contains("writer post"));

    for mode in ["invalid", "fleet"] {
        let f = Fixture::new(mode);
        let mut command = f.command(TOOLS, "claim");
        command
            .env_remove("LOOM_GH_GATEWAY_DIR")
            .args(["--role", "judge", "--sweep-id", "me"]);
        assert!(assert_code(&run(command, None), 1).contains("loom-tools:"));
        assert!(f.read("calls").is_empty());
    }
}

/// #14529: fleet mode with no eligible reader is fail-closed on both
/// routing branches, and the positive route through the same fixture shows
/// each absent-route case differs only by its missing element.
#[test]
fn fleet_mode_without_an_eligible_reader_never_reaches_the_writer() {
    // route_read's no-route arm: a named origin and an adopted gateway
    // (gh-ok matches), but the owner has no profile, so neither the gateway
    // nor a reader pool can answer. Fleet mode must exhaust the read, and
    // claim's only forge call is its separate writer write-scope probe.
    let f = Fixture::new("fleet");
    fs::remove_file(f.gateway.join("acme/hosts.yml")).unwrap();
    assert_code(&f.lease(TOOLS, "claim", "judge", "me", &[]), 5);
    let check = f.lease(TOOLS, "check", "judge", "me", &[]);
    assert_code(&check, 5);
    assert!(check.stdout.starts_with(b"PR_LEASE_STATE=unknown"));
    let calls = f.read("calls");
    assert!(!calls.contains("comments"), "{calls}");
    assert!(!calls.lines().any(|line| line.ends_with(" post")), "{calls}");
    assert!(calls.lines().all(|line| line == "writer permissions"), "{calls}");

    // host_has_read_pool()'s fleet-required disjunct: no repo is derivable
    // (no origin), the gateway directory validates fleet mode but adopts no
    // gateway (no gh-ok marker, so any_live() stays false), and there is no
    // reader pool. The untargeted read must shed with zero forge calls; a
    // writer fallback would read empty rows and answer `free`.
    let f = Fixture::new("fleet");
    fs::remove_file(f.gateway.join("gh-ok")).unwrap();
    let mut git = base("/usr/bin/git", f.tmp.path(), &f.root);
    git.args(["remote", "remove", "origin"]);
    assert!(run(git, None).status.success());
    let check = f.lease(TOOLS, "check", "judge", "me", &[]);
    assert_code(&check, 5);
    assert!(check.stdout.starts_with(b"PR_LEASE_STATE=unknown"));
    assert!(f.read("calls").is_empty(), "{}", f.read("calls"));

    // The positive route: origin, matching marker and the owner profile in
    // place, the same check answers `free` through the gateway.
    let f = Fixture::new("fleet");
    let check = f.lease(TOOLS, "check", "judge", "me", &[]);
    assert_code(&check, 0);
    assert!(check.stdout.starts_with(b"PR_LEASE_STATE=free"));
    assert!(f.read("calls").contains("gateway comments"));
}

#[test]
fn daemon_service_actions_are_refused_by_tools_before_routing() {
    let f = Fixture::new("invalid");
    let mut attach = f.command(TOOLS, "attach");
    attach.args(["--role", "judge", "--sweep-id", "service"]);
    assert!(assert_code(&run(attach, None), 64).contains("daemon service operations"));
    let mut reap = f.command(TOOLS, "reap");
    reap.args([
        "--role",
        "judge",
        "--sweep-id",
        "service",
        "--host",
        "host-00000012",
    ]);
    assert!(assert_code(&run(reap, None), 64).contains("daemon service operations"));
    assert!(f.read("calls").is_empty());

    let mut attach = f.command(DAEMON, "attach");
    attach
        .env_remove("LOOM_GH_GATEWAY_DIR")
        .args(["--role", "judge", "--sweep-id", "service"]);
    assert!(assert_code(&run(attach, None), 3).contains("no live lease"));
    f.seed(201, "judge", "service", 0);
    let mut attach = f.command(DAEMON, "attach");
    attach
        .env_remove("LOOM_GH_GATEWAY_DIR")
        .args(["--role", "judge", "--sweep-id", "service"]);
    assert_code(&run(attach, None), 0);

    let mut reap = f.command(DAEMON, "reap");
    reap.env_remove("LOOM_GH_GATEWAY_DIR").args([
        "--role",
        "judge",
        "--sweep-id",
        "service",
        "--host",
        "host-00000012",
    ]);
    assert_code(&run(reap, None), 0);
    assert!(f.read("calls").contains("writer comments"));
    assert!(f.read("calls").contains("writer release"));
    assert!(!f.read("calls").contains("writer post"));
    let rows = f.read("rows");
    assert!(rows.contains("loom:pr-lease-released role=judge"), "{rows}");
    assert!(rows.contains("reaped=timeout"), "{rows}");
}
