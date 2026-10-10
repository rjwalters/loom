//! Launch-level regression for the Judge/`zai-*` exclusion (#11284): admission
//! must judge the profile `profiles::select` will actually pick, so an explicit
//! `--profile` outranks `LOOM_MODEL_PROFILE` and the harness default here too.

use super::*;
use serial_test::serial;

const KEYS: [&str; 5] = [
    "LOOM_WORKSPACE",
    "LOOM_ROLE",
    "LOOM_RUNTIME",
    "LOOM_RUNTIME_JUDGE",
    "LOOM_MODEL_PROFILE",
];

struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    fn new() -> Self {
        let prior = KEYS.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for key in KEYS {
            std::env::remove_var(key);
        }
        Self(prior)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// A workspace whose `judge` role is pinned to the native `opencode` runtime.
fn judge_workspace(root: &Path) {
    for sub in [".loom/roles", ".loom/runtimes"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(root.join(".loom/config.json"), r#"{"runtimes":{"roles":{"judge":"opencode"}}}"#)
        .unwrap();
    fs::write(
        root.join(".loom/roles/judge.json"),
        r#"{"runtimeRequirements":["loomControl"]}"#,
    )
    .unwrap();
    fs::write(
        root.join(".loom/runtimes/opencode.json"),
        r#"{"runtime":"opencode","capabilities":{"loomControl":"yes"}}"#,
    )
    .unwrap();
}

fn launch(root: &Path, profile: &str) -> Result<(), LaunchError> {
    let args = WorkerArgs {
        scripts_dir: None,
        args: ["--prompt", "/loom:judge 1", "--profile", profile]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
    };
    run_preflight(
        args,
        root,
        None,
        None,
        Some(&crate::forge_egress::policy::PolicySources::default()),
    )
}

/// `LOOM_MODEL_PROFILE` names a non-Z.ai profile, but the launch's `--profile`
/// (which wins in `profiles::select`) is `zai-flash`: refused.
#[test]
#[serial]
fn an_explicit_zai_profile_is_refused_despite_a_non_zai_env_profile() {
    let _env = EnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    judge_workspace(dir.path());
    std::env::set_var("LOOM_WORKSPACE", dir.path());
    std::env::set_var("LOOM_ROLE", "judge");
    std::env::set_var("LOOM_MODEL_PROFILE", "kimi-k2");

    let error = launch(dir.path(), "zai-flash").unwrap_err();
    assert!(
        error
            .message
            .contains(crate::runtime_admission::JUDGE_ZAI_RULE),
        "{}",
        error.message
    );
}

/// The opposite direction: with the default (`zai-flash`) in force, a non-Z.ai
/// `--profile` is what the launch runs on, so admission must accept it. The
/// launch itself is not driven past admission (it would exec a harness); the
/// profile-aware entry point `run_preflight` calls is asserted directly.
#[test]
#[serial]
fn a_non_zai_launch_profile_is_admitted_despite_the_zai_default() {
    let _env = EnvGuard::new();
    let dir = tempfile::tempdir().unwrap();
    judge_workspace(dir.path());
    std::env::set_var("LOOM_MODEL_PROFILE", "zai-flash");

    let blind = crate::runtime_admission::resolve_and_admit(dir.path(), "judge", Some("opencode"));
    assert!(blind.is_err(), "profile-blind admission would wrongly refuse");
    crate::runtime_admission::resolve_and_admit_tap(
        dir.path(),
        "judge",
        "opencode",
        Some("kimi-k2"),
    )
    .expect("the launch's own non-zai profile is admitted");
}
