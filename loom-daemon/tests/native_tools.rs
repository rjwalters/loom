//! Actual shared policy and tool effects; no provider calls.
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(d.path())
        .status()
        .unwrap()
        .success());
    fs::create_dir_all(d.path().join(".loom/worktrees/issue-1")).unwrap();
    fs::write(d.path().join(".loom/worktrees/issue-1/.loom-managed"), "test\n").unwrap();
    fs::write(d.path().join("main.txt"), "original").unwrap();
    d
}
fn call(root: &Path, tool: &str, input: Value) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["runtime-tool", "--workspace"])
        .arg(root)
        .arg("--cwd")
        .arg(root)
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_GUARD_WORKTREE_ISOLATION", "1")
        .env("LOOM_FORCE_SCOPE", "protected")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env_remove("LOOM_WORKTREE_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::to_string(&json!({"tool":tool,"input":input}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    child.wait_with_output().unwrap()
}
#[test]
fn managed_write_edit_read_and_main_denial() {
    let d = fixture();
    let path = ".loom/worktrees/issue-1/result.txt";
    let out = call(d.path(), "write", json!({"path":path,"content":"old\n"}));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(call(d.path(), "edit", json!({"path":path,"oldText":"old","newText":"new"}))
        .status
        .success());
    assert_eq!(fs::read_to_string(d.path().join(path)).unwrap(), "new\n");
    assert!(String::from_utf8_lossy(&call(d.path(), "read", json!({"path":path})).stdout)
        .contains("new"));
    assert!(!call(d.path(), "write", json!({"path":"main.txt","content":"BAD"}))
        .status
        .success());
    assert_eq!(fs::read_to_string(d.path().join("main.txt")).unwrap(), "original");
}
#[test]
fn shell_write_and_protected_branch_are_denied_without_effects() {
    let d = fixture();
    for command in [
        "printf BAD > main.txt",
        "git push --force origin main",
        "git reset --hard HEAD",
    ] {
        let out = call(d.path(), "bash", json!({"command":command}));
        assert!(
            !out.status.success(),
            "must deny {command}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert_eq!(out.status.code(), Some(78));
    }
    assert_eq!(fs::read_to_string(d.path().join("main.txt")).unwrap(), "original");
    let out = call(d.path(), "bash", json!({"command":"git status --porcelain"}));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
}
#[test]
fn unknown_tool_and_ambiguous_edit_fail_closed() {
    let d = fixture();
    assert_eq!(call(d.path(), "task", json!({})).status.code(), Some(78));
    let path = ".loom/worktrees/issue-1/repeated.txt";
    fs::write(d.path().join(path), "same same").unwrap();
    assert!(!call(d.path(), "edit", json!({"path":path,"oldText":"same","newText":"BAD"}))
        .status
        .success());
    assert_eq!(fs::read_to_string(d.path().join(path)).unwrap(), "same same");
}

#[test]
fn native_sweep_uses_profile_and_does_not_inherit_claude_pool_holds() {
    // #4739 convention (mirrors ClearedLoomRuntimeEnv in runtime_admission.rs):
    // ambient LOOM_RUNTIME outranks the config writes below (env > config
    // precedence), so any host that exports it — e.g. a native worker session —
    // would flip the claude half of this test. Isolate it for this test's scope
    // and restore afterwards, including on panic.
    struct RestoreLoomRuntime(Option<String>);
    impl Drop for RestoreLoomRuntime {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_RUNTIME", v),
                None => std::env::remove_var("LOOM_RUNTIME"),
            }
        }
    }
    let _runtime = RestoreLoomRuntime(std::env::var("LOOM_RUNTIME").ok());
    std::env::remove_var("LOOM_RUNTIME");
    let d = fixture();
    fs::write(
        d.path().join(".loom/config.json"),
        r#"{"runtimes":{"default":"pi"},"autonomous":{"modelExperiment":{"mode":"experiment"}}}"#,
    )
    .unwrap();
    let model = loom_daemon::sweep_registry::resolve_autonomous_dispatch_model(d.path(), 1, None);
    assert!(model.model.is_empty(), "{model:?}");
    assert!(model.arm.is_none());
    let holds = loom_daemon::work_finder::pool_preflight::PoolHoldState::new();
    holds.note_pool_dead(d.path(), chrono::Utc::now());
    assert_eq!(holds.held_pool_count(), 1);
    assert!(!holds.observe_root(d.path(), chrono::Utc::now()));
    fs::write(d.path().join(".loom/config.json"), r#"{"runtimes":{"default":"claude"}}"#).unwrap();
    assert!(holds.observe_root(d.path(), chrono::Utc::now()));
}

#[test]
fn broken_missing_and_timed_out_policy_never_executes_the_tool() {
    use std::sync::OnceLock;
    static BIN: OnceLock<std::path::PathBuf> = OnceLock::new();
    let binary = BIN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let bin = dir.join("bash");
        assert!(Command::new("rustc")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/native_guard.rs"))
            .arg("-o")
            .arg(&bin)
            .status()
            .unwrap()
            .success());
        bin
    });
    let d = fixture();
    let worker_pid = "test-worker-8451";
    for mode in ["invalid", "unknown", "crash", "timeout", "missing"] {
        let guard_dir = if mode == "missing" {
            d.path().join("missing-guards")
        } else {
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"))
        };
        let mut command = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        command
            .args(["runtime-tool", "--workspace"])
            .arg(d.path())
            .arg("--cwd")
            .arg(d.path())
            .env("LOOM_NATIVE_GUARD_DIR", guard_dir)
            .env("GUARD_FIXTURE_MODE", mode)
            .env("LOOM_NATIVE_WORKER_PID", worker_pid)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    binary.parent().unwrap().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        if mode == "timeout" {
            // The fixture sleeps 60s; shrink the configurable budget (#8451)
            // instead of waiting out the 20s default on every CI run.
            command.env("LOOM_NATIVE_POLICY_TIMEOUT_SECS", "1");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(br#"{"tool":"write","input":{"path":".loom/worktrees/issue-1/SHOULD_NOT_EXIST","content":"bad"}}"#).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(78), "{mode}: {}", String::from_utf8_lossy(&out.stdout));
        assert!(!d
            .path()
            .join(".loom/worktrees/issue-1/SHOULD_NOT_EXIST")
            .exists());
        // #8451: every non-timeout failure mode here is a guard/policy
        // malfunction, never a denial and never a transient timeout — it must
        // carry the distinct "policy error:" class, not "policy timeout:" or
        // "policy denied:", so a model cannot mistake "the guard is broken"
        // for "retry is reasonable" or "this command is refused".
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if mode == "timeout" {
            assert!(stdout.contains("policy timeout:"), "{mode}: {stdout}");
            assert!(!stdout.contains("policy denied:"), "{mode}: {stdout}");
            assert!(!stdout.contains("policy error:"), "{mode}: {stdout}");
        } else {
            assert!(stdout.contains("policy error:"), "{mode}: {stdout}");
            assert!(!stdout.contains("policy timeout:"), "{mode}: {stdout}");
            assert!(!stdout.contains("policy denied:"), "{mode}: {stdout}");
        }
    }
    // Only the "timeout" iteration above should have incremented this
    // worker's counter (#8451 acceptance criterion 4).
    assert_eq!(loom_daemon::native_tools::guard::policy_timeout_count(d.path(), worker_pid), 1);
    assert_eq!(
        loom_daemon::native_tools::guard::policy_timeout_count(d.path(), "some-other-worker"),
        0
    );
}

#[test]
fn policy_denial_is_reported_distinctly_from_timeout_and_error() {
    let d = fixture();
    let out = call(d.path(), "bash", json!({"command":"git push --force origin main"}));
    assert_eq!(out.status.code(), Some(78));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("policy denied:"), "{stdout}");
    assert!(!stdout.contains("policy timeout:"), "{stdout}");
    assert_eq!(
        loom_daemon::native_tools::guard::policy_timeout_count(d.path(), "any-worker"),
        0
    );
}

#[test]
fn native_policy_timeout_budget_is_configurable_and_bounded() {
    let d = fixture();
    // Env override outranks config (repo-wide env > config > default
    // precedence) and is clamped into [5, 120] rather than honored literally.
    assert_eq!(loom_daemon::native_tools::guard::timeout_secs(d.path()), 20);

    fs::write(
        d.path().join(".loom/config.json"),
        r#"{"guards":{"nativePolicyTimeoutSecs":45}}"#,
    )
    .unwrap();
    assert_eq!(loom_daemon::native_tools::guard::timeout_secs(d.path()), 45);

    fs::write(
        d.path().join(".loom/config.json"),
        r#"{"guards":{"nativePolicyTimeoutSecs":1}}"#,
    )
    .unwrap();
    assert_eq!(loom_daemon::native_tools::guard::timeout_secs(d.path()), 5);

    fs::write(
        d.path().join(".loom/config.json"),
        r#"{"guards":{"nativePolicyTimeoutSecs":99999}}"#,
    )
    .unwrap();
    assert_eq!(loom_daemon::native_tools::guard::timeout_secs(d.path()), 120);

    std::env::set_var(loom_daemon::native_tools::guard::TIMEOUT_ENV, "7");
    assert_eq!(loom_daemon::native_tools::guard::timeout_secs(d.path()), 7);
    std::env::remove_var(loom_daemon::native_tools::guard::TIMEOUT_ENV);
}

#[cfg(unix)]
#[test]
fn terminating_native_tool_terminates_its_shell_descendants() {
    let d = fixture();
    let pidfile = d.path().join(".loom/worktrees/issue-1/child.pid");
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["runtime-tool", "--workspace"])
        .arg(d.path())
        .arg("--cwd")
        .arg(d.path())
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(serde_json::to_string(&json!({"tool":"bash","input":{"command":format!("sleep 60 & echo $! > {}; wait",pidfile.display())}})).unwrap().as_bytes()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !pidfile.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid: i32 = fs::read_to_string(&pidfile)
        .expect("shell started")
        .trim()
        .parse()
        .unwrap();
    // Only signal the processes created by this test.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(!alive, "the shell descendant survived cancellation of the native tool");
}
