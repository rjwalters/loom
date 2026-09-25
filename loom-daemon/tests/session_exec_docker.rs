//! No credentials or model calls. Run explicitly with Docker and a Linux binary:
//! LOOM_TEST_SESSION_IMAGE=ubuntu:24.04 cargo test -p loom-daemon \
//!   --test session_exec_docker -- --ignored --nocapture
//! On macOS set LOOM_TEST_LINUX_BIN to a Linux build of the same source.

use std::fs;
use std::io::Write;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn docker(args: &[&str]) -> Output {
    Command::new("docker").args(args).output().unwrap()
}

struct Fixture {
    name: String,
    root: tempfile::TempDir,
    host_bin: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
    }
}

struct Adapter(Child);
impl Drop for Adapter {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }
}

impl Fixture {
    fn new(supervised: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path = fs::canonicalize(root.path()).unwrap();
        let host_bin = std::env::var_os("LOOM_TEST_HOST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon")));
        let linux_bin = std::env::var_os("LOOM_TEST_LINUX_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| host_bin.clone());
        let name = format!("loom-codex-session-test-{}", uuid::Uuid::new_v4());
        let profile = root_path.join(name.strip_prefix("loom-codex-session-").unwrap());
        fs::create_dir(&profile).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(profile.join("auth.json"), "{\"token\":\"synthetic-test-only\"}").unwrap();
        fs::set_permissions(profile.join("auth.json"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(
            profile.join(".session-managed.json"),
            serde_json::json!({
                "schema_version": 1, "container_name": name, "adopted_at_unix": 0
            })
            .to_string(),
        )
        .unwrap();
        fs::create_dir(root_path.join("bin")).unwrap();
        let worker = root_path.join("bin/codex");
        fs::write(&worker, r##"#!/bin/sh
if [ "$1" = login ]; then exit 0; fi
if [ "$1" = --version ]; then echo 'codex-cli 0.149.1'; exit 0; fi
for job in "$@"; do :; done
case "$job" in
normal) printf 'worker stdout\n'; printf 'session id: 11111111-1111-1111-1111-111111111111\nworker stderr\n' >&2; exit 23;;
*)
  trap '' TERM
  echo $$ > "/tmp/$job-root"
  setsid sh -c 'trap "" TERM; echo $$ > "/tmp/$1-child"; exec sleep 300' sh "$job" &
  wait
  ;;
esac
"##).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();
        let image =
            std::env::var("LOOM_TEST_SESSION_IMAGE").unwrap_or_else(|_| "ubuntu:24.04".into());
        let mount = format!("{}:{}:ro", root_path.display(), root_path.display());
        let binary_mount = format!("{}:/usr/local/bin/loom-daemon:ro", linux_bin.display());
        let path = format!("PATH={}/bin:/usr/local/bin:/usr/bin:/bin", root_path.display());
        let mut args = vec![
            "run",
            "-d",
            "--name",
            &name,
            "--network",
            "none",
            "--entrypoint",
            "/bin/sh",
            "-v",
            &mount,
            "-e",
            &path,
        ];
        if supervised {
            args.extend(["-v", &binary_mount]);
        }
        args.extend([&image, "-c", "exec sleep 600"]);
        let output = docker(&args);
        if !output.status.success() {
            let _ = docker(&["rm", "-f", &name]);
        }
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        Self {
            name,
            root,
            host_bin,
        }
    }

    fn path(&self) -> PathBuf {
        fs::canonicalize(self.root.path()).unwrap()
    }

    fn adapter(&self, job: &str, capture: bool) -> Adapter {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = self.path();
        let mut command = Command::new("bash");
        if job == "killed-owner" {
            // Stand-in daemon: its adapter shell and Rust child survive this
            // parent's SIGKILL, so the stable owner watch must revoke them.
            command.args(["-c", "bash \"$@\" & wait", "launcher"]);
        }
        command
            .arg(repo.join("defaults/scripts/spawn-codex.sh"))
            .args(["-p", job, "--skip-git-repo-check"])
            .current_dir(&root)
            .env("LOOM_DAEMON_SELF_BIN", &self.host_bin)
            .env(
                "LOOM_CODEX_HOME",
                root.join(self.name.strip_prefix("loom-codex-session-").unwrap()),
            )
            .env("LOOM_WORKSPACE", &root)
            .env("LOOM_CODEX_SANDBOX", "read-only")
            .env("LOOM_ROLE", "curator")
            .env("LOOM_SWEEP_NICE", "0")
            .env("HOME", &root)
            .env_remove("CODEX_HOME")
            .env_remove("LOOM_CODEX_PROFILE")
            .env_remove("LOOM_CODEX_NO_EXEC")
            .env_remove("LOOM_CODEX_MODEL")
            .env_remove("LOOM_MODEL")
            .env_remove("LOOM_CODEX_SESSION_EXEC")
            .stdin(Stdio::null())
            .stdout(fs::File::create(root.join(format!("{job}.out"))).unwrap())
            .stderr(fs::File::create(root.join(format!("{job}.err"))).unwrap())
            .process_group(0);
        if capture {
            command.env_remove("LOOM_CODEX_NO_CAPTURE");
        } else {
            command.env("LOOM_CODEX_NO_CAPTURE", "1");
        }
        Adapter(command.spawn().unwrap())
    }

    fn pids(&self, job: &str) -> (i32, i32) {
        let start = Instant::now();
        loop {
            let result = docker(&[
                "exec",
                &self.name,
                "sh",
                "-c",
                &format!("cat /tmp/{job}-root /tmp/{job}-child"),
            ]);
            let pids: Vec<i32> = String::from_utf8_lossy(&result.stdout)
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
            if result.status.success() && pids.len() == 2 {
                return (pids[0], pids[1]);
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "worker failed to start: {}",
                fs::read_to_string(self.path().join(format!("{job}.err"))).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn exists(&self, pid: i32) -> bool {
        docker(&["exec", &self.name, "test", "-e", &format!("/proc/{pid}")])
            .status
            .success()
    }

    fn wait(&self, child: &mut Adapter) -> i32 {
        let start = Instant::now();
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(
                start.elapsed() < Duration::from_secs(6),
                "host did not complete bounded cleanup"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
#[ignore = "disposable real Docker smoke; no credentials or model calls"]
fn adapter_cancels_only_its_own_tree_and_recovers_after_host_death() {
    let fixture = Fixture::new(true);
    let mut normal = fixture.adapter("normal", true);
    assert_eq!(
        fixture.wait(&mut normal),
        23,
        "{}",
        fs::read_to_string(fixture.path().join("normal.err")).unwrap()
    );
    assert_eq!(fs::read(fixture.path().join("normal.out")).unwrap(), b"worker stdout\n");
    let stderr = fs::read_to_string(fixture.path().join("normal.err")).unwrap();
    assert!(stderr.contains("worker stderr"));
    assert!(stderr.contains("spawn-codex: session=11111111-1111-1111-1111-111111111111"));
    assert!(!stderr.contains("loom-session-clean:"));

    let mut sibling = fixture.adapter("sibling", true);
    let sibling_pids = fixture.pids("sibling");
    for capture in [true, false] {
        let job = if capture { "captured" } else { "uncaptured" };
        let mut child = fixture.adapter(job, capture);
        let pids = fixture.pids(job);
        let started = Instant::now();
        unsafe { libc::kill(-(child.0.id() as i32), libc::SIGTERM) };
        assert_eq!(fixture.wait(&mut child), 143);
        // Assert immediately after the caller completes: eventual cleanup is
        // insufficient for the role runner's timeout/slot-release contract.
        assert!(!fixture.exists(pids.0), "worker survived host completion");
        assert!(!fixture.exists(pids.1), "setsid descendant survived host completion");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(fixture.exists(sibling_pids.0) && fixture.exists(sibling_pids.1));
        println!("cooperative {job}: cleanup acknowledged in {:?}", started.elapsed());
    }
    for (job, group) in [
        ("killed-group", true),
        ("killed-shell", false),
        ("killed-owner", false),
    ] {
        let mut child = fixture.adapter(job, true);
        let pids = fixture.pids(job);
        let started = Instant::now();
        let target = child.0.id() as i32 * if group { -1 } else { 1 };
        unsafe { libc::kill(target, libc::SIGKILL) };
        fixture.wait(&mut child);
        while fixture.exists(pids.0) || fixture.exists(pids.1) {
            assert!(started.elapsed() < Duration::from_secs(4), "lease recovery exceeded 4s");
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(fixture.exists(sibling_pids.0) && fixture.exists(sibling_pids.1));
        println!("abrupt {job}: tree reaped in {:?}", started.elapsed());
    }
    let mut stalled = fixture.adapter("stalled", true);
    let stalled_pids = fixture.pids("stalled");
    let started = Instant::now();
    unsafe { libc::kill(-(stalled.0.id() as i32), libc::SIGSTOP) };
    while fixture.exists(stalled_pids.0) || fixture.exists(stalled_pids.1) {
        assert!(started.elapsed() < Duration::from_secs(4), "heartbeat lease did not expire");
        std::thread::sleep(Duration::from_millis(40));
    }
    unsafe { libc::kill(-(stalled.0.id() as i32), libc::SIGCONT) };
    assert_eq!(fixture.wait(&mut stalled), 143);
    assert!(fixture.exists(sibling_pids.0) && fixture.exists(sibling_pids.1));
    println!("stalled heartbeat: tree reaped in {:?}", started.elapsed());
    // Cancellation while the shell/host endpoint is still starting must not
    // leave a delayed invocation after the caller has returned.
    for delay in [0, 10, 50, 150] {
        let job = format!("startup-{delay}");
        let mut child = fixture.adapter(&job, true);
        std::thread::sleep(Duration::from_millis(delay));
        unsafe { libc::kill(-(child.0.id() as i32), libc::SIGTERM) };
        fixture.wait(&mut child);
        std::thread::sleep(Duration::from_millis(150));
        let result = docker(&[
            "exec",
            &fixture.name,
            "sh",
            "-c",
            &format!("cat /tmp/{job}-root /tmp/{job}-child 2>/dev/null"),
        ]);
        for pid in String::from_utf8_lossy(&result.stdout).split_whitespace() {
            assert!(!fixture.exists(pid.parse().unwrap()), "startup cancellation leaked a worker");
        }
    }
    unsafe { libc::kill(-(sibling.0.id() as i32), libc::SIGTERM) };
    assert_eq!(fixture.wait(&mut sibling), 143);
    assert!(docker(&["inspect", "-f", "{{.State.Running}}", &fixture.name])
        .stdout
        .starts_with(b"true"));
}

#[test]
#[ignore = "disposable real Docker smoke; no credentials or model calls"]
fn worker_rejects_missing_expired_cancelled_leases_and_preserves_launch_failure() {
    let fixture = Fixture::new(true);
    for (lease, command, expected) in [
        ("".to_string(), "/bin/true", 143),
        ("1\n".to_string(), "/bin/true", 143),
        ("cancel\n".to_string(), "/bin/true", 143),
        (format!("{}\ncancel\n", epoch_ms() + 2000), "/bin/true", 143),
        (format!("{}\n", epoch_ms() + 2000), "/does-not-exist", 127),
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new("docker")
            .args([
                "exec",
                "-i",
                &fixture.name,
                "loom-daemon",
                "session-exec",
                "worker",
                "--id",
                &id,
                "--",
                command,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(lease.as_bytes()).unwrap();
        // Keep the channel open for a valid lease; EOF itself cancels.
        if lease.is_empty() {
            drop(input);
        } else {
            std::thread::sleep(Duration::from_millis(100));
            drop(input);
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("loom-session-clean:{id}"))
        );
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
#[ignore = "disposable real Docker smoke; no credentials or model calls"]
fn adapter_fails_closed_without_container_protocol() {
    let fixture = Fixture::new(false);
    let mut child = fixture.adapter("never-launched", true);
    assert_eq!(fixture.wait(&mut child), 78);
    assert!(fs::read_to_string(fixture.path().join("never-launched.err"))
        .unwrap()
        .contains("update/recreate the session image"));
    assert!(!docker(&[
        "exec",
        &fixture.name,
        "test",
        "-e",
        "/tmp/never-launched-root"
    ])
    .status
    .success());
}
