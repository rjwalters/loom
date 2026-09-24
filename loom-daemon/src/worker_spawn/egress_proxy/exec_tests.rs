//! Coverage for `loom-daemon worker proxy-exec` (issue #8697): the entry point
//! `spawn-claude.sh`'s contained dispatch uses to keep the real Claude
//! credential out of its container.
//!
//! The container itself is out of reach here (no docker, no provider key), so
//! the assertions are made on the two things that decide what a container
//! would see: the environment of the process this subcommand spawns (docker
//! reads every `-e VAR` from exactly there), and the bytes a real listener
//! forwards to a fake upstream.

use super::*;
use crate::worker_spawn::egress_proxy::tests::{env_lock, fake_upstream, raw_request};
use crate::worker_spawn::egress_proxy::{server, Placeholder};
use std::ffi::{OsStr, OsString};

const REAL: &str = "sk-ant-oat01-fake-real-credential-8697";

fn args(command: &[&str]) -> ExecArgs {
    ExecArgs {
        credential_env: "CLAUDE_CODE_OAUTH_TOKEN".into(),
        upstream: "https://api.anthropic.com".into(),
        header: HeaderStyle::AuthorizationBearer,
        base_url_env: vec!["ANTHROPIC_BASE_URL".into()],
        provider: "claude".into(),
        docker_workspace: None,
        forward_env: vec!["LOOM_TOKEN_NAME".into()],
        command: command.iter().map(OsString::from).collect(),
    }
}

fn argv(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn env_with(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

/// The value the spawned command's environment will hold for `name`:
/// `Some(Some(v))` explicitly set, `Some(None)` explicitly removed, `None`
/// inherited unchanged.
fn child_env<'a>(command: &'a Command, name: &str) -> Option<Option<&'a OsStr>> {
    command
        .get_envs()
        .find(|(k, _)| *k == OsStr::new(name))
        .map(|(_, v)| v)
}

// ------------------------------------------------------------ substitution

#[test]
fn the_command_sees_only_a_placeholder_and_the_proxy_base_url() {
    let env = env_with(&[
        ("CLAUDE_CODE_OAUTH_TOKEN", REAL),
        ("LOOM_TOKEN_NAME", "account-a"),
    ]);
    let (prepared, command) = build(&args(&["docker", "run", "img"]), &env).unwrap();

    let token = child_env(&command, "CLAUDE_CODE_OAUTH_TOKEN")
        .flatten()
        .expect("the credential variable must be explicitly assigned")
        .to_string_lossy()
        .into_owned();
    assert!(token.starts_with(PLACEHOLDER_PREFIX), "{token}");
    assert_ne!(token, REAL);

    let base = child_env(&command, "ANTHROPIC_BASE_URL")
        .flatten()
        .expect("the base URL must be re-pointed at the proxy")
        .to_string_lossy()
        .into_owned();
    assert!(base.starts_with("http://"), "{base}");
    assert!(base.ends_with(&format!(":{}", prepared.bound.addr().port())), "{base}");

    // Nothing the child is explicitly handed, and nothing in its argv, carries
    // the real value.
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            assert!(!value.to_string_lossy().contains(REAL), "{key:?} leaks the credential");
        }
    }
    assert!(!command
        .get_args()
        .any(|a| a.to_string_lossy().contains(REAL)));
    // A non-secret variable is left to plain inheritance.
    assert!(child_env(&command, "LOOM_TOKEN_NAME").is_none());

    assert_eq!(prepared.injection.withheld, vec!["CLAUDE_CODE_OAUTH_TOKEN".to_string()]);
    let marker = prepared.dispatch_marker();
    assert!(marker.starts_with("# LOOM_EGRESS_PROXY "), "{marker}");
    assert!(marker.contains("upstream=https://api.anthropic.com"), "{marker}");
    assert!(marker.contains("header=authorization-bearer"), "{marker}");
    assert!(!marker.contains(REAL), "{marker}");
    assert!(!marker.contains(PLACEHOLDER_PREFIX), "{marker}");
}

#[test]
fn any_other_variable_carrying_the_real_value_is_removed_from_the_child() {
    let env = env_with(&[
        ("CLAUDE_CODE_OAUTH_TOKEN", REAL),
        ("ANTHROPIC_AUTH_TOKEN", REAL),
        ("LOOM_STRAY_COPY", &format!("prefix-{REAL}")),
        ("UNRELATED", "value"),
    ]);
    let (_prepared, command) = build(&args(&["true"]), &env).unwrap();
    assert_eq!(child_env(&command, "ANTHROPIC_AUTH_TOKEN"), Some(None));
    assert_eq!(child_env(&command, "LOOM_STRAY_COPY"), Some(None));
    assert!(child_env(&command, "UNRELATED").is_none());
}

/// AC1 at the process level: spawn the built command for real and read its
/// whole environment back — `/proc/self/environ` where it exists, `env`
/// elsewhere. Docker's `-e VAR` reads exactly this environment, so a value
/// absent here cannot reach the container.
#[cfg(unix)]
#[test]
fn the_spawned_process_environment_never_holds_the_real_credential() {
    // `build` scrubs against the passed-in snapshot; seed the real process
    // environment the same way spawn-claude.sh's `export` would, so the child
    // would inherit it if substitution did not override it.
    let _g = env_lock();
    std::env::set_var("EXEC_TEST_COPY_8697", REAL);
    let env = env_with(&[
        ("CLAUDE_CODE_OAUTH_TOKEN", REAL),
        ("EXEC_TEST_COPY_8697", REAL),
    ]);
    let script =
        "if [ -r /proc/self/environ ]; then tr '\\0' '\\n' < /proc/self/environ; else env; fi";
    let (_prepared, mut command) = build(&args(&["sh", "-c", script]), &env).unwrap();
    let output = command.output().unwrap();
    std::env::remove_var("EXEC_TEST_COPY_8697");
    let environ = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(!environ.contains(REAL), "the real credential reached the child environment");
    assert!(
        environ.contains(&format!("CLAUDE_CODE_OAUTH_TOKEN={PLACEHOLDER_PREFIX}")),
        "{environ}"
    );
    assert!(environ.contains("ANTHROPIC_BASE_URL=http://"), "{environ}");
}

// ------------------------------------------------------------ fail closed

#[test]
fn an_unset_or_blank_credential_is_refused_never_launched() {
    for env in [
        env_with(&[]),
        env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", "")]),
        env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", "   ")]),
    ] {
        let error = build(&args(&["true"]), &env).unwrap_err();
        assert_eq!(error.code, 78);
        assert!(error.message.contains("refusing to launch"), "{}", error.message);
    }
}

#[test]
fn a_value_that_is_already_a_placeholder_is_refused() {
    let nested = Placeholder::generate();
    let env = env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", nested.as_str())]);
    let error = build(&args(&["true"]), &env).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error.message.contains("already holds a Loom placeholder"), "{}", error.message);
    assert!(!error.message.contains(nested.as_str()), "the placeholder must not be echoed");
}

#[test]
fn malformed_arguments_are_refused() {
    let env = env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", REAL)]);

    let mut bad = args(&["true"]);
    bad.credential_env = "NOT A NAME".into();
    assert_eq!(build(&bad, &env).unwrap_err().code, 78);

    let mut bad = args(&["true"]);
    bad.base_url_env = vec!["CLAUDE_CODE_OAUTH_TOKEN".into()];
    assert_eq!(build(&bad, &env).unwrap_err().code, 78);

    let mut bad = args(&["true"]);
    bad.upstream = "https://user:pw@api.anthropic.com".into();
    let error = build(&bad, &env).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(!error.message.contains(REAL));

    let bad = args(&[]);
    assert_eq!(build(&bad, &env).unwrap_err().code, 78);
}

#[test]
fn the_cli_shape_spawn_claude_uses_parses() {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct Harness {
        #[command(flatten)]
        exec: ExecArgs,
    }
    // Exactly what spawn-claude.sh passes: everything else is Claude's default.
    let parsed = Harness::try_parse_from([
        "proxy-exec",
        "--docker-workspace",
        "/ws",
        "--",
        "docker",
        "run",
        "--rm",
        "-e",
        "LOOM_ROLE",
        "img",
    ])
    .unwrap()
    .exec;
    assert_eq!(parsed.credential_env, "CLAUDE_CODE_OAUTH_TOKEN");
    assert_eq!(parsed.upstream, "https://api.anthropic.com");
    assert_eq!(parsed.header, HeaderStyle::AuthorizationBearer);
    assert_eq!(parsed.base_url_env, vec!["ANTHROPIC_BASE_URL".to_string()]);
    assert_eq!(parsed.forward_env, vec!["LOOM_TOKEN_NAME".to_string()]);
    assert_eq!(parsed.docker_workspace.as_deref(), Some(std::path::Path::new("/ws")));
    assert_eq!(parsed.command[0], "docker");
    assert_eq!(parsed.command[3], "-e");

    // Explicit flags still override the defaults, for any other adapter.
    let parsed = Harness::try_parse_from([
        "proxy-exec",
        "--credential-env",
        "OPENAI_API_KEY",
        "--upstream",
        "https://api.openai.com",
        "--header",
        "authorization-bearer",
        "--base-url-env",
        "OPENAI_BASE_URL",
        "--",
        "true",
    ])
    .unwrap()
    .exec;
    assert_eq!(parsed.credential_env, "OPENAI_API_KEY");
    assert_eq!(parsed.base_url_env, vec!["OPENAI_BASE_URL".to_string()]);
    assert!(parsed.docker_workspace.is_none());
    assert!(parse_header("x-api-key").is_ok());
    assert!(parse_header("basic").is_err());
}

// ------------------------------------------------------ --docker-workspace

#[test]
fn without_docker_workspace_the_argv_is_passed_through_untouched() {
    let env = env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", REAL)]);
    let (_prepared, command) = build(&args(&["docker", "run", "--rm", "img"]), &env).unwrap();
    assert_eq!(argv(&command), vec!["run", "--rm", "img"]);
}

#[test]
fn docker_workspace_splices_forwards_add_host_and_pool_masks_after_run() {
    let ws = tempfile::tempdir().unwrap();
    let root = ws.path();
    std::fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    std::fs::create_dir_all(root.join(".loom/api-keys")).unwrap();
    std::fs::create_dir_all(root.join("shared-pool")).unwrap();
    let env = env_with(&[
        ("CLAUDE_CODE_OAUTH_TOKEN", REAL),
        ("LOOM_SHARED_TOKENS_DIR", root.join("shared-pool").to_str().unwrap()),
    ]);
    let mut exec_args = args(&[
        "docker",
        "run",
        "--rm",
        "-e",
        "LOOM_TOKEN_NAME",
        "img",
        "cmd",
    ]);
    exec_args.docker_workspace = Some(root.to_path_buf());
    let (_prepared, command) = build(&exec_args, &env).unwrap();
    let mask =
        |p: &str| format!("type=tmpfs,destination={},tmpfs-mode=0555", root.join(p).display());
    assert_eq!(
        argv(&command),
        vec![
            "run".to_string(),
            "--mount".into(),
            mask(".loom/tokens"),
            "--mount".into(),
            mask(".loom/api-keys"),
            "--mount".into(),
            mask("shared-pool"),
            "-e".into(),
            "CLAUDE_CODE_OAUTH_TOKEN".into(),
            "-e".into(),
            "ANTHROPIC_BASE_URL".into(),
            // LOOM_TOKEN_NAME is already forwarded by the caller: not doubled.
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
            "--rm".into(),
            "-e".into(),
            "LOOM_TOKEN_NAME".into(),
            "img".into(),
            "cmd".into(),
        ]
    );
}

#[test]
fn docker_workspace_masks_only_existing_pools_inside_the_workspace() {
    let ws = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".loom/api-keys")).unwrap();
    let env = env_with(&[
        ("CLAUDE_CODE_OAUTH_TOKEN", REAL),
        ("LOOM_SHARED_TOKENS_DIR", outside.path().to_str().unwrap()),
    ]);
    let mut exec_args = args(&["docker", "run", "img"]);
    exec_args.docker_workspace = Some(ws.path().to_path_buf());
    let (_prepared, command) = build(&exec_args, &env).unwrap();
    let mounts: Vec<String> = argv(&command)
        .windows(2)
        .filter(|w| w[0] == "--mount")
        .map(|w| w[1].clone())
        .collect();
    // .loom/tokens does not exist (docker would create it on the host); the
    // shared pool is outside the workspace (never mounted at all).
    assert_eq!(
        mounts,
        vec![format!(
            "type=tmpfs,destination={},tmpfs-mode=0555",
            ws.path().join(".loom/api-keys").display()
        )]
    );
    assert!(argv(&command).contains(&"LOOM_TOKEN_NAME".to_string()));
}

#[test]
fn docker_workspace_refuses_a_command_that_is_not_docker_run() {
    let env = env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", REAL)]);
    let mut exec_args = args(&["docker", "exec", "c"]);
    exec_args.docker_workspace = Some("/ws".into());
    assert_eq!(build(&exec_args, &env).unwrap_err().code, 78);

    let mut exec_args = args(&["docker", "run", "img"]);
    exec_args.docker_workspace = Some("/ws".into());
    exec_args.forward_env = vec!["NOT A NAME".into()];
    assert_eq!(build(&exec_args, &env).unwrap_err().code, 78);
}

// ------------------------------------------------- round trip + lifetime (AC3)

/// The OAuth-bearer shape Claude Code 2.x actually sends through
/// `ANTHROPIC_BASE_URL` (observed against a recording listener while building
/// #8697): `Authorization: Bearer <CLAUDE_CODE_OAUTH_TOKEN>` plus an
/// `anthropic-beta` list that includes `oauth-2025-04-20`. The swap must
/// replace the bearer and pass the beta header through untouched — without
/// it the provider rejects an OAuth token outright.
#[tokio::test]
async fn an_oauth_request_is_swapped_and_the_placeholder_dies_with_the_launch() {
    let (upstream_addr, seen) = fake_upstream("{\"ok\":true}").await;
    let env = env_with(&[("CLAUDE_CODE_OAUTH_TOKEN", REAL)]);
    let mut exec_args = args(&["true"]);
    exec_args.upstream = format!("http://{upstream_addr}");
    let (prepared, command) = build(&exec_args, &env).unwrap();
    let placeholder = child_env(&command, "CLAUDE_CODE_OAUTH_TOKEN")
        .flatten()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let Prepared {
        registry, bound, ..
    } = prepared;
    let addr = bound.addr();
    let listener = bound.into_tokio().unwrap();
    tokio::spawn(server::serve(listener, registry.clone()));

    let request = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nhost: {addr}\r\nauthorization: Bearer {placeholder}\r\n\
         anthropic-beta: claude-code-20250219,oauth-2025-04-20\r\nanthropic-version: 2023-06-01\r\n\
         content-type: application/json\r\ncontent-length: 2\r\n\r\n{{}}"
    );
    let response = raw_request(addr, &request).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    {
        let seen = seen.lock().unwrap();
        let head = seen[0].head.to_ascii_lowercase();
        assert!(head.contains(&format!("authorization: bearer {}", REAL.to_ascii_lowercase())));
        assert!(head.contains("anthropic-beta: claude-code-20250219,oauth-2025-04-20"));
        assert!(!seen[0].head.contains(&placeholder));
    }

    // What `run_with_proxy` does the instant the child exits.
    registry.close_all();
    let response = raw_request(addr, &request).await;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    assert!(response.contains("closed_launch"), "{response}");
    assert_eq!(seen.lock().unwrap().len(), 1, "nothing forwarded after close");
}
