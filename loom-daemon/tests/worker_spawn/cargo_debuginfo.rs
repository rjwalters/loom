//! #11190: the dev/test debuginfo cap `worker_spawn::run` injects, end to end
//! through `spawn-worker`, for sweep, role-run and native dispatch.
use super::{config, legacy, worker};
use std::process::Command;

/// #11190: the dev/test debuginfo cap the seam injects, as the worker sees it.
fn debuginfo(root: &std::path::Path, runtime: &str, tweak: &dyn Fn(&mut Command)) -> [String; 2] {
    let out = spawn(root, runtime, tweak);
    let text = String::from_utf8_lossy(&out.stdout);
    ["DEV", "TEST"].map(|p| {
        let prefix = format!("child_env CARGO_PROFILE_{p}_DEBUG=");
        text.lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .unwrap_or_default()
            .to_string()
    })
}
/// The `# LOOM_CARGO_DEBUGINFO` marker the seam writes to the worker log (stderr
/// here, as no `--log` is given).
fn marker(root: &std::path::Path, tweak: &dyn Fn(&mut Command)) -> String {
    let out = spawn(root, "claude", tweak);
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|l| l.starts_with("# LOOM_CARGO_DEBUGINFO"))
        .unwrap_or_default()
        .to_string()
}
fn spawn(
    root: &std::path::Path,
    runtime: &str,
    tweak: &dyn Fn(&mut Command),
) -> std::process::Output {
    let mut c = worker(root, runtime);
    c.env("CARGO_HOME", root.join("cargo-home"))
        .env_remove("LOOM_CARGO_DEBUGINFO")
        .env_remove("CARGO_PROFILE_DEV_DEBUG")
        .env_remove("CARGO_PROFILE_TEST_DEBUG")
        .env("FIXTURE_PRINT_ENV", "CARGO_PROFILE_DEV_DEBUG,CARGO_PROFILE_TEST_DEBUG")
        .args(["-p", "hello"]);
    tweak(&mut c);
    let out = c.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out
}
#[test]
fn cargo_debuginfo_is_capped_for_sweep_and_role_runs_unless_already_chosen() {
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "claude");
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("curator.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
    let capped = [
        "line-tables-only".to_string(),
        "line-tables-only".to_string(),
    ];
    let none = || [String::new(), String::new()];
    // Sweep dispatch (claim-owning builder), a role-runner tick, and a native
    // harness all converge on the one seam.
    let sweep = |c: &mut Command| {
        c.env("LOOM_SWEEP_CLAIM_OWNED", "11190")
            .env("LOOM_ROLE", "builder");
    };
    let role = |c: &mut Command| {
        c.env("LOOM_ROLE", "curator");
    };
    assert_eq!(debuginfo(d.path(), "claude", &sweep), capped);
    assert_eq!(debuginfo(d.path(), "claude", &role), capped);
    assert_eq!(debuginfo(d.path(), "pi", &|_| {}), capped);
    // An ambient value wins; `test` then inherits the operator's `dev` choice.
    let ambient = |c: &mut Command| {
        sweep(c);
        c.env("CARGO_PROFILE_DEV_DEBUG", "full");
    };
    assert_eq!(debuginfo(d.path(), "claude", &ambient), ["full".to_string(), String::new()]);
    // The worker log records what was set and what was kept, and why.
    assert_eq!(
        marker(d.path(), &sweep),
        "# LOOM_CARGO_DEBUGINFO dev=line-tables-only test=line-tables-only (#11190)"
    );
    assert_eq!(
        marker(d.path(), &ambient),
        "# LOOM_CARGO_DEBUGINFO dev=kept(ambient) test=kept(inherits-dev) (#11190)"
    );
    // The env override picks the level.
    let limited = |c: &mut Command| {
        c.env("LOOM_CARGO_DEBUGINFO", "limited");
    };
    assert_eq!(
        debuginfo(d.path(), "claude", &limited),
        ["limited", "limited"].map(String::from)
    );
    // A repo's explicit profile is respected, per profile.
    std::fs::write(d.path().join("Cargo.toml"), "[workspace]\n[profile.test]\ndebug = 2\n")
        .unwrap();
    assert_eq!(
        debuginfo(d.path(), "claude", &sweep),
        ["line-tables-only".to_string(), String::new()]
    );
    std::fs::write(d.path().join("Cargo.toml"), "[workspace]\n[profile.dev]\ndebug = true\n")
        .unwrap();
    assert_eq!(debuginfo(d.path(), "claude", &sweep), none());
    std::fs::remove_file(d.path().join("Cargo.toml")).unwrap();
    // The per-repo opt-out turns it off entirely.
    config(d.path(), serde_json::json!({"cargo":{"debuginfo":"full"}}));
    assert_eq!(debuginfo(d.path(), "claude", &sweep), none());
    assert_eq!(debuginfo(d.path(), "pi", &role), none());
    assert!(marker(d.path(), &sweep).starts_with("# LOOM_CARGO_DEBUGINFO off setting=full"));
    // A JSON `false` reads as "no cap", not as an unrecognized level.
    config(d.path(), serde_json::json!({"cargo":{"debuginfo":false}}));
    assert_eq!(debuginfo(d.path(), "claude", &sweep), none());
}
