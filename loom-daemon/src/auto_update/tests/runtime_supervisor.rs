//! Isolated Linux process evidence for #11042. The download is a fixture;
//! the tick, IpcRollTrigger, pause completion and process exit are real.
use super::*;
use std::process::{Command, Stdio};

const CHILD: &str = "auto_update::tests::floor_roll::runtime_supervisor::runtime_child";

struct StageProbe {
    inner: ArtifactFakeProbe,
    binary: PathBuf,
    staged: PathBuf,
}
impl AutoUpdateProbe for StageProbe {
    fn resolve_artifact(&self) -> ArtifactResolution {
        self.inner.resolve_artifact()
    }
    fn check(&self) -> UpdateCheck {
        self.inner.check()
    }
    fn is_tree_clean(&self) -> Option<bool> {
        self.inner.is_tree_clean()
    }
    fn in_flight_sweeps(&self) -> usize {
        0
    }
    fn rebuild(&mut self, low: bool) -> RebuildOutcome {
        self.inner.rebuild(low)
    }
    fn fetch_artifact(&mut self, tag: &str, low: bool) -> RebuildOutcome {
        std::fs::copy(&self.binary, &self.staged).unwrap();
        self.inner.fetch_artifact(tag, low)
    }
}

#[test]
#[ignore = "isolated AWS Linux runtime evidence; requires LOOM_11261_DAEMON_BIN"]
fn isolated_supervisor_boundary() {
    let binary = PathBuf::from(std::env::var_os("LOOM_11261_DAEMON_BIN").expect("built daemon"));
    assert!(binary.is_absolute());
    let sandbox = tempfile::tempdir().unwrap();
    for mode in ["unsupervised", "supervised"] {
        let root = sandbox.path().join(mode);
        std::fs::create_dir(&root).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", CHILD, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("LOOM_11261_ROOT", &root)
            .env("LOOM_11261_DAEMON_BIN", &binary)
            .env("LOOM_11261_MODE", mode)
            .env("LOOM_GH_BIN", "/bin/false")
            .env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
            .env("LOOM_AUTO_UPDATE_STATE_DIR", &root)
            .env("LOOM_DAEMON_RESTART_VERIFY", "0")
            .env("LOOM_ROLE_RUNNER", "0")
            .env("LOOM_WORK_FINDER", "0")
            .env("LOOM_EPIC_SUPERVISOR", "0")
            .env("LOOM_WORKSPACE", &root)
            .env("LOOM_WORKSPACES_PATH", root.join("workspaces.json"))
            .env("LOOM_SWEEPS_JOURNAL_PATH", root.join("sweeps.json"))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let first_pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(60);
        let result = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("child exceeded bound");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(result.success(), "{mode}: {result}");
        assert!(
            root.join("tick-complete").exists(),
            "exit must follow tick, not a skipped helper"
        );
        if mode == "supervised" {
            assert!(!root.join("unexpected-return").exists(), "real pause must exit the child");
            let staged = root.join("staged-daemon");
            assert_eq!(std::fs::read(&staged).unwrap(), std::fs::read(&binary).unwrap());
            // The isolated supervisor's restart action resolves the installed
            // path afresh, never the old child executable.
            let socket = root.join("relaunched.sock");
            let mut restarted = Command::new(&staged)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &root)
                .current_dir(&root)
                .env("LOOM_SOCKET_PATH", &socket)
                .env("LOOM_PID_FILE", root.join("relaunched.pid"))
                .env("LOOM_WORKSPACE", &root)
                .env("LOOM_WORKSPACES_PATH", root.join("workspaces.json"))
                .env("LOOM_WORKTREE_ROOT", root.join("worktrees"))
                .env("LOOM_SWEEPS_JOURNAL_PATH", root.join("sweeps.json"))
                .env("LOOM_WATCHES_PATH", root.join("watches.json"))
                .env("LOOM_WATCH_RESULTS_LOG", root.join("watch-results.log"))
                .env("LOOM_SESSION_FALLBACK_ROOT_FILE", root.join("fallback.json"))
                .env("LOOM_BUILD_SLOT_DIR", root.join("slots"))
                .env("LOOM_AUTONOMY_MARKER", root.join("autonomy"))
                .env("LOOM_AUTO_UPDATE_STATE_DIR", root.join("update-state"))
                .env("LOOM_ROLE_RUNNER", "0")
                .env("LOOM_WORK_FINDER", "0")
                .env("LOOM_EPIC_SUPERVISOR", "0")
                .env("LOOM_SESSION_RECONCILE", "0")
                .env("LOOM_TARGET_ORPHAN_RECLAIM", "0")
                .env("LOOM_NO_RESTORE", "1")
                .env("LOOM_GH_BIN", "/bin/false")
                .env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let new_pid = restarted.id();
            assert_ne!(new_pid, first_pid);
            let ready_by = Instant::now() + Duration::from_secs(30);
            let ready = loop {
                if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                    break true;
                }
                if restarted.try_wait().unwrap().is_some() || Instant::now() >= ready_by {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            let executable = std::fs::canonicalize(format!("/proc/{new_pid}/exe"));
            let _ = restarted.kill();
            let _ = restarted.wait();
            assert!(ready, "staged daemon must bind its isolated socket");
            assert_eq!(executable.unwrap(), staged.canonicalize().unwrap());
            println!(
                "supervised: old_pid={first_pid} new_pid={new_pid} socket_ready=true staged={}",
                staged.display()
            );
        }
    }
}

#[test]
#[ignore = "child only, invoked by isolated_supervisor_boundary"]
fn runtime_child() {
    let root = PathBuf::from(std::env::var_os("LOOM_11261_ROOT").expect("child sandbox"));
    let supervised = std::env::var("LOOM_11261_MODE").unwrap() == "supervised";
    if supervised {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "systemd");
    } else {
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _entered = runtime.enter();
    let bus = Arc::new(EventBus::new());
    let drain = Arc::new(DrainState::new());
    let pool = Arc::new(WorkspacePool::new(bus.clone(), runtime.handle().clone()));
    let trigger = IpcRollTrigger::new(drain, pool, root.clone(), bus, runtime.handle().clone());
    assert_eq!(trigger.unsupervised().is_none(), supervised);
    let mut state = floored(&root, Floor::Below);
    let fetches = Arc::new(AtomicUsize::new(0));
    let status = AutoUpdateStatus::new(true);
    let mut p = StageProbe {
        inner: probe(newest(RUNNING), &fetches),
        binary: PathBuf::from(std::env::var_os("LOOM_11261_DAEMON_BIN").unwrap()),
        staged: root.join("staged-daemon"),
    };
    let first = run_tick(&mut state, &status, &mut p, &trigger, Duration::ZERO, DEFER);
    assert_eq!(first.roll_armed, supervised);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    if !supervised {
        for _ in 0..3 {
            run_tick(&mut state, &status, &mut p, &trigger, Duration::ZERO, DEFER);
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        p.inner.artifact = resolved(artifact("0.19.901", Some(RUNNING), Some(SHA_A), Some(SHA_B)));
        for _ in 0..3 {
            run_tick(&mut state, &status, &mut p, &trigger, Duration::ZERO, DEFER);
        }
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        println!(
            "unsupervised: same_target_fetches=1 changed_target_total=2 pid={}",
            std::process::id()
        );
    }
    std::fs::write(root.join("tick-complete"), "real trigger completed").unwrap();
    if supervised {
        runtime.block_on(tokio::time::sleep(Duration::from_secs(45)));
        std::fs::write(root.join("unexpected-return"), "pause did not exit").unwrap();
        panic!("supervised pause did not exit");
    }
}
