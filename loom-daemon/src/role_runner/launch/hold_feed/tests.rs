//! #11453 end to end: deterministic launch failures trip the **tap**, never
//! the role.
//!
//! Drives the real tick loop ([`spawn_role_task`]) over the real
//! [`ScriptRoleInvocationRunner`]: runtime preflight, the ordered preference
//! walk, admission, the spawn, the toolless verdict (#8448) and this module's
//! feed. The workspace's Doctor prefers `opencode:zai-flash` over `claude`,
//! both taps are selectable, and a fake `spawn-worker.sh` makes every
//! OpenCode launch exit 0 with the toolless event stream #8448 recorded,
//! while a Claude launch succeeds.
//!
//! The #6201 tests in `role_runner/tests.rs` are deliberately untouched: they
//! drive a `FakeRunner` that bypasses the walk and the launch path, so the
//! hold cannot reach them. This is their sibling for the path that does.
use crate::role_runner::{
    new_in_progress_guard, spawn_role_task, RoleInvocationRunner, RoleSpec, RoleTickOutcome,
    ScriptRoleInvocationRunner,
};
use crate::runtime_preference::launch_failure_hold;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// #8448's live OpenCode receipt: a step, prose about missing tools, exit 0.
const TOOLLESS_STREAM: &str = concat!(
    r#"{"type":"step_start"}"#,
    "\n",
    r#"{"type":"text","text":"I do not have tools named loom_write, loom_bash, or loom_edit"}"#,
);

/// Everything that could pin the runtime, supply a credential or redirect the
/// spawn script, cleared (#10767) — plus the hold's own knobs.
const ISOLATED: [&str; 12] = [
    "LOOM_RUNTIME",
    "LOOM_RUNTIME_DOCTOR",
    "LOOM_MODEL_PROFILE",
    "LOOM_NATIVE_AUTH_FILE",
    crate::api_keys_pool::paths::SHARED_API_KEYS_DIR_ENV,
    "ZAI_API_KEY",
    "ZHIPU_API_KEY",
    crate::sweep_registry::SPAWN_BIN_ENV,
    "LOOM_USE_WRAPPER",
    "LOOM_ROLE_RUNNER_MODEL",
    launch_failure_hold::HOLD_SECS_ENV,
    launch_failure_hold::THRESHOLD_ENV,
];

struct Env(Vec<(&'static str, Option<String>)>);

impl Env {
    fn new() -> Self {
        let prior = ISOLATED
            .iter()
            .map(|key| {
                let prior = std::env::var(key).ok();
                std::env::remove_var(key);
                (*key, prior)
            })
            .collect();
        launch_failure_hold::reset_for_tests();
        Self(prior)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        launch_failure_hold::reset_for_tests();
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The bundled role/runtime manifests, an enabled `zai` seat,
/// `rolePreference.doctor = [opencode:zai-flash, claude]`, and the fake
/// spawn script described in the module doc.
fn doctor_workspace_with(root: &Path, opencode_output: &str) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (from, to) in [
        ("defaults/roles", ".loom/roles"),
        ("defaults/runtimes", ".loom/runtimes"),
    ] {
        fs::create_dir_all(root.join(to)).unwrap();
        for entry in fs::read_dir(repo.join(from)).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                fs::copy(&path, root.join(to).join(path.file_name().unwrap())).unwrap();
            }
        }
    }
    let scripts = root.join(".loom/scripts");
    fs::create_dir_all(&scripts).unwrap();
    for runtime in ["claude", "codex"] {
        write_executable(&scripts.join(format!("spawn-{runtime}.sh")), "#!/bin/sh\n");
    }
    let launches = root.join("launches");
    write_executable(
        &scripts.join("spawn-worker.sh"),
        &format!(
            "#!/bin/sh\necho \"$LOOM_RUNTIME\" >> '{}'\n\
             if [ \"$LOOM_RUNTIME\" = opencode ]; then printf '%s\\n' '{opencode_output}'; fi\n\
             exit 0\n",
            launches.display()
        ),
    );
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(root);
    crate::api_keys_pool::registry::add(&pool, "zai", "alpha", "ZAI_API_KEY", "fake", false)
        .unwrap();
    crate::api_keys_pool::registry::set_enabled(&pool, "zai", "alpha", true).unwrap();
    let list = serde_json::json!([{"runtime": "opencode", "modelProfile": "zai-flash"}, "claude"]);
    fs::write(
        root.join(".loom/config.json"),
        serde_json::json!({"runtimes": {"rolePreference": {"doctor": list}}}).to_string(),
    )
    .unwrap();
}

/// Each tick's outcome, the tap its launch landed on, and its preference
/// marker.
type Ticks = Arc<Mutex<Vec<(RoleTickOutcome, Option<String>, String)>>>;

/// The real runner, recording every tick. After `limit` ticks it stops
/// launching, so a tick still in flight when the loop is aborted spawns
/// nothing after the test's environment guard is gone.
struct Recording {
    inner: ScriptRoleInvocationRunner,
    calls: Arc<AtomicUsize>,
    ticks: Ticks,
    limit: usize,
}

impl RoleInvocationRunner for Recording {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        if self.calls.load(Ordering::SeqCst) >= self.limit {
            return RoleTickOutcome::Success;
        }
        let outcome = self.inner.invoke(role, prompt);
        let stamp = self
            .inner
            .resolved_launch()
            .and_then(|launch| launch.preference);
        let marker = stamp.as_ref().map(|s| s.marker.clone()).unwrap_or_default();
        let tap = stamp.map(|s| s.tap);
        self.ticks
            .lock()
            .unwrap()
            .push((outcome.clone(), tap, marker));
        self.calls.fetch_add(1, Ordering::SeqCst);
        outcome
    }
}

async fn drive_four_ticks(
    opencode_output: &str,
) -> (tempfile::TempDir, Vec<(RoleTickOutcome, Option<String>, String)>) {
    let _env = Env::new();
    let workspace = tempfile::tempdir().unwrap();
    doctor_workspace_with(workspace.path(), opencode_output);
    let ws = crate::write_scope_test_support::WritableRoot::register(workspace.path());
    // A registered root is a git checkout, which refuses an in-tree token
    // pool (#9135): Claude's account lives in the shared pool, as it does on
    // a real host.
    let _claude = crate::write_scope_test_support::SharedTokenPool::one_account();
    let calls = Arc::new(AtomicUsize::new(0));
    let ticks: Ticks = Arc::default();
    let runner = Recording {
        inner: ScriptRoleInvocationRunner::new(workspace.path().to_path_buf())
            .with_timeout(Duration::from_secs(60))
            .with_load_per_core_override(0.0)
            .with_gh_bin(ws.gh.clone()),
        calls: calls.clone(),
        ticks: ticks.clone(),
        limit: 4,
    };
    let spec = RoleSpec {
        name: "doctor",
        prompt: "/loom:doctor",
        default_interval_secs: 1,
        interval_default: true,
    };
    let handle = spawn_role_task(
        runner,
        spec,
        Duration::from_millis(20),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        workspace.path().to_path_buf(),
        new_in_progress_guard(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    while calls.load(Ordering::SeqCst) < 4 {
        assert!(
            Instant::now() < deadline,
            "the role stopped ticking after {} tick(s): {:?}",
            calls.load(Ordering::SeqCst),
            ticks.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.abort();

    let ticks = ticks.lock().unwrap().clone();
    (workspace, ticks)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn deterministic_launch_failures_trip_the_tap_not_the_role() {
    let (workspace, ticks) = drive_four_ticks(TOOLLESS_STREAM).await;
    // N = 2 toolless launches on the preferred tap...
    for (outcome, tap, _) in &ticks[..2] {
        assert_eq!(tap.as_deref(), Some("opencode:zai-flash"), "{ticks:?}");
        let RoleTickOutcome::Failure(detail) = outcome else {
            panic!("a toolless launch is a failed tick: {ticks:?}");
        };
        assert!(detail.contains("toolless launch"), "{detail}");
    }
    // ...then the role keeps ticking, on the fallback tap.
    for (outcome, tap, marker) in &ticks[2..] {
        assert_eq!(tap.as_deref(), Some("claude"), "{ticks:?}");
        assert_eq!(*outcome, RoleTickOutcome::Success, "{ticks:?}");
        // The held tap's skip reason, naming the class, rides the marker.
        assert!(
            marker.contains("unavailable(launch-failure-hold: toolless-launch x2"),
            "{marker}"
        );
    }
    let launches = fs::read_to_string(workspace.path().join("launches")).unwrap();
    let launches: Vec<&str> = launches.lines().collect();
    assert_eq!(launches[..2], ["opencode", "opencode"], "{launches:?}");
    assert!(launches[2..].iter().all(|r| *r == "claude"), "{launches:?}");
}

/// #11727: the exact shape of the 2026-10-12 outage. Every tool call is denied
/// by the role-tool-policy hook because `check` exited 2; the child exits 0.
const HOOK_DENIED_STREAM: &str = "exec\n/bin/bash -lc 'cat SKILL.md' in /w\n exited 1 in 0ms:\n\
    BLOCKED [role-tool-policy]: role 'doctor' is restricted, and loom-daemon role-tool-policy \
    check could not answer (exit 2), so this command is denied (#8256).";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn a_hook_denied_run_falls_back_to_the_next_runtime() {
    let (workspace, ticks) = drive_four_ticks(HOOK_DENIED_STREAM).await;
    for (outcome, tap, _) in &ticks[..2] {
        assert_eq!(tap.as_deref(), Some("opencode:zai-flash"), "{ticks:?}");
        let RoleTickOutcome::Failure(detail) = outcome else {
            panic!("a hook-denied run is a failed tick: {ticks:?}");
        };
        assert!(detail.contains("guard hook denied every tool call"), "{detail}");
    }
    for (outcome, tap, marker) in &ticks[2..] {
        assert_eq!(tap.as_deref(), Some("claude"), "{ticks:?}");
        assert_eq!(*outcome, RoleTickOutcome::Success, "{ticks:?}");
        assert!(marker.contains("launch-failure-hold: hook-denied x2"), "{marker}");
    }
    let launches = fs::read_to_string(workspace.path().join("launches")).unwrap();
    assert!(launches.lines().skip(2).all(|r| r == "claude"), "{launches:?}");
}
