#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! W5: a spawned session is told where the host's forge-call sink is, and
//! never inherits the facade's "already booked" marker.

use super::*;
use crate::agent_session::spawn::{spawn_agent, SpawnOptions};
use crate::agent_session::testing::FakeEnv;
use crate::agent_session::CmdOutput;
use crate::forge_call_stats::set_test_sink_dir;
use std::path::PathBuf;

/// Run `body` with this thread's sink at `dir`.
fn with_sink<T>(dir: Option<PathBuf>, body: impl FnOnce() -> T) -> T {
    set_test_sink_dir(dir);
    let out = body();
    set_test_sink_dir(None);
    out
}

fn repo(tmp: &tempfile::TempDir, wrapper: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".loom/roles")).unwrap();
    std::fs::write(root.join(".loom/roles/builder.md"), "role").unwrap();
    if wrapper {
        let scripts = root.join(".loom/scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let path = scripts.join("claude-wrapper.sh");
        std::fs::write(&path, "#!/bin/bash\nexec claude \"$@\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    root
}

fn spawned(root: &Path) -> FakeEnv {
    let env = FakeEnv::new();
    env.set("has-session", CmdOutput::ok(""));
    env.set("list-panes", CmdOutput::ok("4242\n"));
    env.set_claude_running(true);
    let opts = SpawnOptions {
        role: "builder".into(),
        name: "builder-1".into(),
        verify_timeout: 3,
        auto_accept_bypass: false,
        ..Default::default()
    };
    assert_eq!(spawn_agent(&env, &opts, root).status, "spawned");
    env
}

#[test]
fn the_ledger_vars_are_the_resolved_sink_and_a_blank_booked_marker() {
    let sink = PathBuf::from("/var/host-tmp/loom-forge-call-stats");
    let vars = with_sink(Some(sink), ledger_vars);
    assert_eq!(
        vars,
        [
            ("LOOM_FORGE_CALL_STATS_DIR", "/var/host-tmp/loom-forge-call-stats".to_string()),
            ("LOOM_GH_BOOKED", String::new()),
        ]
    );
    // A relative sink would mean a different directory in the session's cwd.
    let relative = with_sink(Some(PathBuf::from("rel/sink")), ledger_vars);
    assert!(Path::new(&relative[0].1).is_absolute(), "{relative:?}");
    assert!(relative[0].1.ends_with("rel/sink"), "{relative:?}");
    // A disabled sink stays disabled in the session: never the session tmp.
    assert_eq!(with_sink(None, ledger_vars)[0].1, "off");
}

#[test]
fn a_session_gets_the_host_sink_beside_its_private_tmpdir() {
    // The terminal path loops over exactly this list.
    let sink = PathBuf::from("/var/host-tmp/loom-forge-call-stats");
    let all = with_sink(Some(sink), || vars(Path::new("/repo/.loom/claude-config/b-1")));
    let names: Vec<&str> = all.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        names,
        [
            "CLAUDE_CONFIG_DIR",
            "TMPDIR",
            "LOOM_FORGE_CALL_STATS_DIR",
            "LOOM_GH_BOOKED"
        ]
    );
    assert_eq!(all[1].1, "/repo/.loom/claude-config/b-1/tmp");
    assert_eq!(all[2].1, "/var/host-tmp/loom-forge-call-stats");
    assert!(
        !all[2].1.starts_with(&all[1].1),
        "the sink is the host's, not under the session TMPDIR"
    );
    assert_eq!(all[3].1, "", "a real agent call is never marked already-booked");
}

#[test]
fn the_prefix_quotes_the_sink_path_for_the_shell() {
    let prefix = with_sink(Some(PathBuf::from("/tmp/it's here/sink")), ledger_prefix);
    assert_eq!(
        prefix,
        "LOOM_FORGE_CALL_STATS_DIR='/tmp/it'\"'\"'s here/sink' LOOM_GH_BOOKED='' "
    );
}

#[test]
fn spawn_agent_exports_the_sink_and_blanks_the_booked_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(&tmp, true);
    let sink = tmp.path().join("host-tmp/loom-forge-call-stats");
    let env = with_sink(Some(sink.clone()), || spawned(&root));
    let sink = sink.to_string_lossy().into_owned();

    // On the tmux session, beside the TMPDIR that would otherwise move it…
    let tmpdir = env.session_env("TMPDIR").unwrap();
    assert!(tmpdir.ends_with(".loom/claude-config/builder-1/tmp"), "{tmpdir}");
    assert_eq!(env.session_env("LOOM_FORGE_CALL_STATS_DIR").as_deref(), Some(sink.as_str()));
    assert_eq!(env.session_env("LOOM_GH_BOOKED").as_deref(), Some(""));
    assert!(
        !env.saw("-u LOOM_GH_BOOKED"),
        "never tmux -u: the server environment's value would leak back in"
    );
    assert!(env.session_env("CLAUDE_CONFIG_DIR").is_some());

    // …and on the wrapper command line, which is where TMPDIR reaches the
    // already-running shell.
    let sent = env.sent_command().unwrap();
    assert!(sent.contains(&format!("TMPDIR='{tmpdir}'")), "{sent}");
    assert!(sent.contains(&format!("LOOM_FORGE_CALL_STATS_DIR='{sink}' ")), "{sent}");
    assert!(sent.contains("LOOM_GH_BOOKED='' "), "{sent}");
}

#[test]
fn the_wrapperless_command_is_unchanged_and_the_session_env_still_carries_both() {
    // Without the wrapper the command line carries no TMPDIR either: the
    // session environment is the only channel, for both.
    let tmp = tempfile::tempdir().unwrap();
    let root = repo(&tmp, false);
    let sink = tmp.path().join("host-tmp/loom-forge-call-stats");
    let env = with_sink(Some(sink.clone()), || spawned(&root));
    assert_eq!(
        env.sent_command().unwrap(),
        "claude --dangerously-skip-permissions \"/loom:builder\""
    );
    assert_eq!(
        env.session_env("LOOM_FORGE_CALL_STATS_DIR"),
        Some(sink.to_string_lossy().into_owned())
    );
    assert_eq!(env.session_env("LOOM_GH_BOOKED").as_deref(), Some(""));
}
