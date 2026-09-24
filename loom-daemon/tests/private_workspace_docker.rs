//! Credential-free real Docker lifecycle/containment proof. Explicitly invoked
//! in CI; no model calls or production account profiles are used.
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

#[path = "private_workspace_docker/auth.rs"]
mod auth;
#[path = "private_workspace_docker/dispatch.rs"]
mod dispatch;

#[track_caller]
fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "fixture command failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
#[track_caller]
fn docker(args: &[&str]) -> String {
    checked(Command::new("docker").args(args).output().unwrap())
}

struct Fixture {
    root: tempfile::TempDir,
    image: String,
    server: String,
    names: Vec<String>,
    host: PathBuf,
    repository: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let logs = Command::new("docker")
                .args(["logs", &self.server])
                .output()
                .unwrap();
            eprintln!(
                "fixture forge diagnostics: {} {}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr)
            );
        }
        for name in &self.names {
            if std::thread::panicking() {
                let diagnostic = Command::new("docker")
                    .args([
                        "inspect",
                        "--format",
                        "{{json .HostConfig.Mounts}} {{json .Mounts}}",
                        &format!("loom-codex-session-{name}"),
                    ])
                    .output()
                    .unwrap();
                eprintln!("fixture mounts: {}", String::from_utf8_lossy(&diagnostic.stdout));
            }
            let _ = Command::new("docker")
                .args(["rm", "-f", &format!("loom-codex-session-{name}")])
                .output();
            let _ = Command::new("docker")
                .args(["volume", "rm", &format!("loom-codex-workspace-{name}")])
                .output();
        }
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.server])
            .output();
        let _ = Command::new("docker")
            .args(["image", "rm", &self.image])
            .output();
    }
}
impl Fixture {
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.host);
        cmd.args(["accounts", "--workspace"])
            .arg(self.root.path().join("registry"))
            .args(args)
            .env("LOOM_CODEX_PROFILE_ROOT", self.root.path().join("profiles"))
            .env("GH_CONFIG_DIR", self.root.path().join("forge"));
        for key in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GITEA_TOKEN",
            "FORGE_TOKEN",
            "GITEA_USERNAME",
            "GH_HOST",
            "LOOM_ACCOUNT_SCOPE",
            "LOOM_RUNTIME",
        ] {
            cmd.env_remove(key);
        }
        cmd.env("GITEA_USERNAME", auth::USERNAME)
            .env("GITEA_TOKEN", auth::PASSWORD);
        cmd
    }
    #[track_caller]
    fn cli(&self, args: &[&str]) -> String {
        checked(self.command(args).output().unwrap())
    }
    #[track_caller]
    fn start(&self, name: &str) -> Value {
        serde_json::from_str(&self.cli(&[
            "session",
            "start",
            name,
            "--private-clone",
            &self.repository,
            "--image",
            &self.image,
            "--json",
        ]))
        .unwrap()
    }
    #[track_caller]
    fn exec(&self, name: &str, command: &str) -> String {
        docker(&[
            "exec",
            &format!("loom-codex-session-{name}"),
            "sh",
            "-c",
            command,
        ])
    }
    fn job(&self, name: &str, kind: &str, command: &str) -> Command {
        self.command(&[
            "session",
            "job",
            name,
            "--kind",
            kind,
            "--owner",
            "docker-proof",
            "--",
            "sh",
            "-c",
            command,
        ])
    }
    fn new() -> Self {
        Self::with_adapters(false)
    }
    fn with_adapters(adapters: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let host = std::env::var_os("LOOM_TEST_HOST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon")));
        let worker = std::env::var_os("LOOM_TEST_LINUX_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| host.clone());
        let mut f = Self {
            root,
            image: format!("loom-private-test:{tag}"),
            server: format!("loom-private-forge-{tag}"),
            names: vec![
                format!("test-{tag}-a"),
                format!("test-{tag}-b"),
                format!("test-{tag}-c"),
            ],
            host,
            repository: String::new(),
        };
        for path in [
            "registry/.loom",
            "profiles",
            "forge",
            "context",
            "source",
            "host-sibling",
        ] {
            std::fs::create_dir_all(f.root.path().join(path)).unwrap();
        }
        std::fs::write(f.root.path().join("host-sibling/keep"), "host fixture").unwrap();
        let source = f.root.path().join("source");
        let git = |args: &[&str]| {
            checked(
                Command::new("git")
                    .arg("-C")
                    .arg(&source)
                    .args([
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(args)
                    .output()
                    .unwrap(),
            )
        };
        git(&["init", "-b", "main"]);
        std::fs::write(source.join("file"), "base").unwrap();
        if adapters {
            let defaults = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("defaults");
            for (src, dest) in [
                ("runtimes", ".loom/runtimes"),
                ("scripts", ".loom/scripts"),
                ("hooks", ".loom/hooks"),
                ("roles", ".loom/roles"),
                (".claude/commands/loom", ".claude/commands/loom"),
            ] {
                dispatch::copy_tree(&defaults.join(src), &source.join(dest));
            }
            std::fs::write(
                source.join(".gitignore"),
                ".loom/sweep-checkpoint/\n.loom/logs/\n.loom/private-jobs/\n.loom/locks/\n",
            )
            .unwrap();
            std::fs::write(source.join(".loom/config.json"), r#"{"forge":{"type":"github"}}"#)
                .unwrap();
        }
        git(&["add", "."]);
        git(&["commit", "-m", "fixture"]);
        let context = f.root.path().join("context");
        git(&[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            context.join("repo.git").to_str().unwrap(),
        ]);
        checked(
            Command::new("git")
                .arg("-C")
                .arg(context.join("repo.git"))
                .arg("update-server-info")
                .output()
                .unwrap(),
        );
        checked(
            Command::new("git")
                .arg("-C")
                .arg(context.join("repo.git"))
                .args(["config", "http.receivepack", "true"])
                .output()
                .unwrap(),
        );
        std::fs::copy(worker, context.join("loom-daemon")).unwrap();
        std::fs::write(context.join("codex"), dispatch::CODEX).unwrap();
        std::fs::write(context.join("gh"), dispatch::GH).unwrap();
        std::fs::write(context.join("serve.py"), auth::SERVER).unwrap();
        std::fs::write(context.join("Dockerfile"), r#"FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y --no-install-recommends git tini python3 openssl ca-certificates jq coreutils && rm -rf /var/lib/apt/lists/*
COPY loom-daemon /usr/local/bin/loom-daemon
COPY codex gh /usr/local/bin/
COPY repo.git /srv/repo.git
COPY serve.py /srv/serve.py
RUN chmod 0755 /usr/local/bin/loom-daemon /usr/local/bin/codex /usr/local/bin/gh && openssl req -x509 -newkey rsa:2048 -nodes -keyout /srv/key.pem -out /srv/cert.pem -days 1 -subj /CN=fixture && git config --system user.name Fixture && git config --system user.email fixture@example.invalid
ENV GIT_SSL_NO_VERIFY=true
WORKDIR /srv
"#).unwrap();
        docker(&["build", "-q", "-t", &f.image, context.to_str().unwrap()]);
        checked(
            Command::new("docker")
                .args([
                    "run",
                    "-d",
                    "--name",
                    &f.server,
                    "--env",
                    "FIXTURE_GITEA_AUTH",
                    "--env",
                    "FIXTURE_GITHUB_AUTH",
                    &f.image,
                    "python3",
                    "/srv/serve.py",
                ])
                .env("FIXTURE_GITEA_AUTH", format!("{}:{}", auth::USERNAME, auth::PASSWORD))
                .env("FIXTURE_GITHUB_AUTH", format!("x-access-token:{}", auth::GH_TOKEN))
                .output()
                .unwrap(),
        );
        let ip = docker(&[
            "inspect",
            "--format",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            &f.server,
        ]);
        f.repository = format!("https://{}:8443/gitea/repo.git", ip.trim());
        let auth = f.root.path().join("synthetic-auth.json");
        std::fs::write(&auth, "synthetic-credential-free-fixture").unwrap();
        for name in &f.names {
            f.cli(&[
                "import",
                "codex",
                name,
                "--auth-file",
                auth.to_str().unwrap(),
            ]);
        }
        f
    }
}

#[test]
#[ignore = "requires Docker; explicitly run by CI"]
fn private_clones_contain_writes_reuse_and_serialize_all_job_kinds() {
    let f = Fixture::new();
    let failed = &f.names[2];
    let failure = f
        .command(&[
            "session",
            "start",
            failed,
            "--private-clone",
            &f.repository,
            "--base",
            "missing-base",
            "--image",
            &f.image,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let retained: Value =
        serde_json::from_str(&f.cli(&["session", "status", failed, "--json"])).unwrap();
    assert_eq!(retained["workspace_mode"], "private-clone");
    f.cli(&["session", "stop", failed, "--json"]);
    assert!(f
        .root
        .path()
        .join("profiles")
        .join(failed)
        .join("auth.json")
        .exists());
    let a = &f.names[0];
    let b = &f.names[1];
    let status = f.start(a);
    assert_eq!(status["workspace_mode"], "private-clone");
    assert_eq!(status["private_root"], "/workspace/repo");
    f.start(b);
    let peer = format!("loom-codex-session-{failed}");
    for mount in [
        format!(
            "type=bind,src={},dst=/peer-profile",
            f.root.path().join("profiles").join(a).display()
        ),
        format!("type=volume,src=loom-codex-workspace-{a},dst=/peer-workspace"),
    ] {
        docker(&[
            "run",
            "-d",
            "--name",
            &peer,
            "--mount",
            &mount,
            "--entrypoint",
            "/bin/sleep",
            &f.image,
            "infinity",
        ]);
        let sharing = f.job(a, "role", "true").output().unwrap();
        assert!(!sharing.status.success());
        assert!(String::from_utf8_lossy(&sharing.stderr).contains("attached to another container"));
        docker(&["rm", "-f", &peer]);
    }

    assert_eq!(
        f.exec(a, "git -C /workspace/repo rev-parse --git-common-dir")
            .trim(),
        ".git"
    );
    assert_eq!(f.exec(a, "test -d /workspace/repo/.git && test ! -e /workspace/repo/.git/objects/info/alternates && echo private").trim(), "private");
    // An identical absolute path may be writable in private /tmp (Linux
    // fixtures also live under /tmp). Check host integrity, not mkdir failure.
    for path in [
        f.root.path().join("source"),
        f.root.path().join("host-sibling"),
    ] {
        let command = format!("test ! -e '{0}' && if mkdir -p '{0}' 2>/dev/null; then printf attack > '{0}/file'; printf attack > '{0}/keep'; fi", path.display());
        f.exec(a, &command);
    }
    let private_tmp = format!("/tmp/{}/source", f.server);
    f.exec(a, &format!("mkdir -p '{private_tmp}' && printf private > '{private_tmp}/file'"));
    assert_eq!(std::fs::read_to_string(f.root.path().join("source/file")).unwrap(), "base");
    assert!(!f.root.path().join("source/keep").exists());
    assert!(!f.root.path().join("host-sibling/file").exists());
    f.exec(a, "test ! -e /var/run/docker.sock && test ! -e /run/containerd/containerd.sock");
    f.exec(a, "echo only-a > /workspace/cache/account-a");
    f.exec(b, "test ! -e /workspace/cache/account-a");
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("host-sibling/keep")).unwrap(),
        "host fixture"
    );
    checked(f.job(a, "role", "git -C /workspace/repo worktree add -b fixture-work /workspace/fixture-work && test -f /workspace/fixture-work/file").output().unwrap());
    f.cli(&["session", "stop", a, "--json"]);
    assert!(f
        .root
        .path()
        .join("profiles")
        .join(a)
        .join("auth.json")
        .exists());
    f.start(a);
    f.exec(a, "test -f /workspace/fixture-work/file && test -f /workspace/cache/account-a");
    // A supervised role owns the same lease consulted by a full sweep and
    // interactive jobs; another account remains available at the same time.
    let mut role = f
        .job(a, "role", "echo ready; sleep 3")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = role.stdout.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout).read_line(&mut line);
        let _ = ready_tx.send((result.is_ok(), line));
    });
    let (read, line) = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("role did not start");
    assert!(read && line.trim() == "ready");
    for kind in ["sweep", "interactive"] {
        let result = f.job(a, kind, "true").output().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("busy"));
    }
    checked(f.job(b, "sweep", "true").output().unwrap());
    assert!(role.wait().unwrap().success());
    assert!(!f
        .command(&["session", "attach", a])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!f
        .command(&["session", "shell", a])
        .output()
        .unwrap()
        .status
        .success());
    // Dirty and unpublished work refuse reuse while preserving bytes/refs.
    checked(
        f.job(a, "interactive", "echo unfinished > /workspace/repo/file")
            .output()
            .unwrap(),
    );
    assert_eq!(f.exec(b, "cat /workspace/repo/file").trim(), "base");
    let dirty = f.job(a, "sweep", "true").output().unwrap();
    assert!(!dirty.status.success());
    assert!(String::from_utf8_lossy(&dirty.stderr).contains("dirty"));
    assert_eq!(f.exec(a, "cat /workspace/repo/file").trim(), "unfinished");
    f.exec(a, "git -C /workspace/repo restore file");
    checked(f.job(a, "sweep", "git -C /workspace/repo switch -c unpublished; echo commit > /workspace/repo/file; git -C /workspace/repo commit -am unpublished").output().unwrap());
    let tip = f.exec(a, "git -C /workspace/repo rev-parse HEAD");
    let unpublished = f.job(a, "role", "true").output().unwrap();
    assert!(!unpublished.status.success());
    assert!(String::from_utf8_lossy(&unpublished.stderr).contains("unpushed"));
    assert_eq!(f.exec(a, "git -C /workspace/repo rev-parse HEAD"), tip);
    // An unleased surviving child also blocks stale-record reclamation.
    docker(&[
        "exec",
        "-d",
        &format!("loom-codex-session-{a}"),
        "sleep",
        "300",
    ]);
    let stale = f.job(a, "role", "true").output().unwrap();
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("surviving"));
    let lease = f
        .root
        .path()
        .join("profiles/.private-sessions")
        .join(a)
        .join("job.json");
    assert!(lease.exists());
    let original = format!("loom-codex-session-{a}");
    let renamed = format!("loom-codex-session-{failed}");
    docker(&["rename", &original, &renamed]);
    let start = f
        .command(&[
            "session",
            "start",
            a,
            "--private-clone",
            &f.repository,
            "--image",
            &f.image,
        ])
        .output()
        .unwrap();
    assert!(!start.status.success());
    assert!(String::from_utf8_lossy(&start.stderr).contains("original container ID"));
    let stop = f.command(&["session", "stop", a]).output().unwrap();
    assert!(!stop.status.success());
    assert!(String::from_utf8_lossy(&stop.stderr).contains("original container ID"));
    assert!(lease.exists());
    docker(&["rename", &renamed, &original]);
}
