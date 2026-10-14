//! #10331: a dispatched worker gets the agent `gh` front's shim directory
//! first on `PATH` (so plain `gh` reads are ETag-revalidated), and
//! `LOOM_GH_SHIM=0` leaves `PATH` untouched. Split from `worker_spawn.rs`,
//! which sits at the file-size threshold; same native-seam fixture.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;

fn worker(root: &Path, shim_base: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["spawn-worker", "--", "-p", "hello"])
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_RUNTIME", "pi")
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env_remove("LOOM_GH_SHIM")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_NATIVE_TOOLS_DIR", fixture().parent().unwrap().join("state"))
        .env_remove("LOOM_NATIVE_AUTH_FILE")
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_GH_SHIM_BASE", shim_base)
        .env("PATH", "/usr/bin:/bin")
        .env("FIXTURE_PRINT_ENV", "PATH");
    c
}

fn child_path(out: &std::process::Output) -> String {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("child_env PATH="))
        .unwrap()
        .to_string()
}

#[test]
fn worker_path_starts_with_the_gh_front_shim() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().join("shim-base");
    let path = child_path(&worker(d.path(), &base).output().unwrap());
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    assert!(dirs[0].starts_with(&base), "{path}");
    assert_eq!(&dirs[1..], [PathBuf::from("/usr/bin"), PathBuf::from("/bin")], "{path}");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));
    assert_eq!(std::fs::read_link(dirs[0].join("gh")).unwrap(), bin.canonicalize().unwrap());
}

#[test]
fn loom_gh_shim_0_leaves_worker_path_alone() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().join("shim-base");
    let out = worker(d.path(), &base)
        .env("LOOM_GH_SHIM", "0")
        .output()
        .unwrap();
    assert_eq!(child_path(&out), "/usr/bin:/bin");
    assert!(!base.exists());
}

/// #10607: the worker's agent `gh` front books into this host's (the
/// spawner's) sink — named explicitly, as every tmux session's is (W5,
/// `agent_session::isolation`), so a TMPDIR the runtime pins cannot send its
/// rows somewhere the daemon never reads — and an `off` sink stays off.
#[test]
fn the_worker_env_names_the_spawners_sink_dir() {
    let d = tempfile::tempdir().unwrap();
    let host_tmp = d.path().join("host-tmp");
    std::fs::create_dir_all(&host_tmp).unwrap();
    let env_of = |extra: &[(&str, &str)]| {
        let mut c = worker(d.path(), &d.path().join("shim-base"));
        let out = c
            .env("TMPDIR", &host_tmp)
            .env_remove("LOOM_FORGE_CALL_STATS_DIR")
            .env("LOOM_GH_BOOKED", "1")
            .env("FIXTURE_PRINT_ENV", "PATH,LOOM_FORGE_CALL_STATS_DIR,LOOM_GH_BOOKED")
            .envs(extra.iter().copied())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let get = |name: &str| {
            let prefix = format!("child_env {name}=");
            stdout
                .lines()
                .find_map(|l| l.strip_prefix(prefix.as_str()))
                .unwrap()
                .to_string()
        };
        (get("LOOM_FORGE_CALL_STATS_DIR"), get("LOOM_GH_BOOKED"))
    };
    let sink = host_tmp.join("loom-forge-call-stats");
    let (named, booked) = env_of(&[]);
    assert_eq!(named, sink.display().to_string());
    assert_eq!(booked, "", "a worker never inherits the facade's booked marker");
    assert!(sink.is_dir(), "created owner-only up front");
    let (named, _) = env_of(&[("LOOM_FORGE_CALL_STATS_DIR", "off")]);
    assert_eq!(named, "off", "an off host sink is exported as off, never a fallback");
}
