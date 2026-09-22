//! Regression tests for the native worker seam. No provider calls or shell fixtures.
use std::process::Command;
#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;
fn worker(root: &std::path::Path, runtime: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["spawn-worker", "--"])
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_RUNTIME", runtime)
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        // Never let a test reach the operator's real `~/.loom/api-keys` (#8401).
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_NATIVE_TOOLS_DIR", fixture().parent().unwrap().join("state"))
        .env_remove("LOOM_NATIVE_AUTH_FILE")
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        // The OpenCode adapter probes `--version` before exec (#8438). Every test
        // that does not say otherwise runs against a 1.x-shaped CLI, so the
        // pre-existing OpenCode tests passing unmodified IS the "1.18.x behavior
        // is unchanged" evidence.
        .env("FIXTURE_VERSION", OPENCODE_V1)
        .env_remove("FIXTURE_VERSION_EXIT");
    c
}
const OPENCODE_V1: &str = "1.18.31";
/// Exactly what `opencode --version` prints on 2.0.10.
const OPENCODE_V2: &str = "opencode v2.0.10";
/// The harness argv, in order, as the fixture's `arg=<Debug>` lines.
fn argv(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("arg="))
        .map(str::to_string)
        .collect()
}
fn quoted(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| format!("{a:?}")).collect()
}
#[test]
fn pi_translates_headless_flags_and_keeps_prompt_literal() {
    let d = tempfile::tempdir().unwrap();
    let out = worker(d.path(), "pi")
        .args([
            "--use-wrapper",
            "--dangerously-skip-permissions",
            "--effort",
            "max",
            "-p",
            "- $(touch SHOULD_NOT_EXIST) `echo literal`\nsecond line",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    for arg in [
        "--provider",
        "zai",
        "--model",
        "glm-5.3-flash",
        "--mode",
        "json",
        "--thinking",
        "max",
    ] {
        assert!(text.contains(&format!("arg={arg:?}")), "{text}");
    }
    assert!(text.contains("$(touch SHOULD_NOT_EXIST)"));
    assert!(!d.path().join("SHOULD_NOT_EXIST").exists());
    assert!(!text.contains("arg=\"--use-wrapper\""));
}
#[test]
fn opencode_selects_coding_plan_and_preserves_nonzero_and_log_streams() {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("worker.log");
    let out = worker(d.path(), "opencode")
        .env("FIXTURE_EXIT", "42")
        .args(["--log", log.to_str().unwrap(), "-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(42), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.contains("zai-coding-plan/glm-5.3-flash"));
    assert!(text.contains("fixture stderr"));
    assert!(text.contains("LOOM_RUNTIME_RESOLVED runtime=opencode"));
    // #8456: the env default rides native-harness dispatch too, not just the
    // legacy shell adapters.
    assert!(text.contains("CARGO_INCREMENTAL=0"), "{text}");
}
#[test]
fn invalid_model_effort_and_unknown_flags_fail_before_launch() {
    let d = tempfile::tempdir().unwrap();
    for args in [
        vec!["--model", "sonnet"],
        vec!["--effort", "off"],
        vec!["--api-key", "never-print-this"],
        vec!["--prompt"],
    ] {
        let out = worker(d.path(), "pi").args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(78));
        assert!(!String::from_utf8_lossy(&out.stderr).contains("never-print-this"));
        assert!(out.stdout.is_empty());
    }
}
#[test]
fn missing_binary_is_127_and_builder_uses_guarded_tools() {
    let d = tempfile::tempdir().unwrap();
    let out = worker(d.path(), "pi")
        .env("LOOM_PI_BIN", d.path().join("missing"))
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    let out = worker(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")), "pi")
        .env("LOOM_ROLE", "builder")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("--no-builtin-tools"));
}
#[cfg(unix)]
#[test]
fn exec_preserves_pid_and_signal_death() {
    use std::os::unix::process::ExitStatusExt;
    let d = tempfile::tempdir().unwrap();
    let child = worker(d.path(), "pi")
        .args(["-p", "hello"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let out = child.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("pid={pid}")));
    let out = worker(d.path(), "pi")
        .env("FIXTURE_SIGNAL", "1")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.signal(), Some(15));
}

fn config(root: &std::path::Path, value: serde_json::Value) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), value.to_string()).unwrap();
}
fn legacy(root: &std::path::Path, runtime: &str) {
    let scripts = root.join(".loom/scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::copy(fixture(), scripts.join(format!("spawn-{runtime}.sh"))).unwrap();
}
#[test]
fn legacy_defaults_config_precedence_argv_and_isolation_are_preserved() {
    let d = tempfile::tempdir().unwrap();
    for runtime in ["claude", "codex"] {
        legacy(d.path(), runtime);
    }
    let out = worker(d.path(), "claude")
        .env_remove("LOOM_RUNTIME")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env_remove("LOOM_DAEMON_LOG")
        .env_remove("LOOM_TEST_ALLOW_SYSTEMD")
        .args(["-p", "hello world", "--unknown=literal"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for expected in [
        "arg=\"hello world\"",
        "arg=\"--unknown=literal\"",
        "LOOM_RUNTIME=claude",
        "LOOM_TEST_ALLOW_SYSTEMD=0",
        "CARGO_INCREMENTAL=0",
        "loom-worker-isolation-",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    config(d.path(), serde_json::json!({"runtimes":{"default":"codex"}}));
    let out = worker(d.path(), "claude")
        .env_remove("LOOM_RUNTIME")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("LOOM_RUNTIME=codex"));
    let out = worker(d.path(), "claude")
        .env("LOOM_DAEMON_LOG", "explicit.log")
        .env("LOOM_TEST_ALLOW_SYSTEMD", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("LOOM_RUNTIME=claude"));
    assert!(text.contains("LOOM_DAEMON_LOG=explicit.log"));
    assert!(text.contains("LOOM_TEST_ALLOW_SYSTEMD=1"));
    // #8456: CARGO_INCREMENTAL is the one env default with no caller-override
    // form — an inherited value would silently re-enable the orphaned
    // incremental-state / non-cacheable-sccache failure this exists to stop.
    assert!(text.contains("CARGO_INCREMENTAL=0"), "{text}");
}
#[test]
fn models_are_independent_of_harness_and_profiles_can_be_extended_in_config() {
    let d = tempfile::tempdir().unwrap();
    for harness in ["pi", "opencode"] {
        let out = worker(d.path(), harness)
            .args([
                "--model",
                "example/model-v2",
                "--effort",
                "high",
                "-p",
                "hello",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("glm-5.3-flash"));
        assert!(String::from_utf8_lossy(&out.stderr).contains("\"provider\":\"example\""));
    }
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"comparison","modelProfiles":{"comparison":{"model":"m3","providers":{"pi":"minimax","opencode":"minimax-coding-plan"},"effort":"high"}}}}),
    );
    for (harness, provider) in [("pi", "minimax"), ("opencode", "minimax-coding-plan")] {
        let out = worker(d.path(), harness)
            .args(["-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(&format!("\"provider\":\"{provider}\""))
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("\"model\":\"m3\""));
    }
}
#[test]
fn role_instructions_are_expanded_and_gate_cannot_be_bypassed_by_slash_prompt() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("curator.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
    std::fs::write(roles.join("curator.md"), "Role task: $ARGUMENTS").unwrap();
    std::fs::write(d.path().join("CLAUDE.md"), "Repository rule marker").unwrap();
    let out = worker(d.path(), "pi")
        .args(["-p", "/loom:curator 8362"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Role task: 8362"));
    assert!(text.contains("Repository rule marker"));
    std::fs::write(roles.join("builder.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    for runtime in ["pi", "opencode"] {
        let out = worker(d.path(), runtime)
            .args(["-p", "/loom:builder 8362"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(78));
        assert!(String::from_utf8_lossy(&out.stderr).contains("mcp"));
    }
}
#[test]
fn role_runner_probe() {
    let Ok(root) = std::env::var("LOOM_TEST_NATIVE_ROLE_ROOT") else {
        return;
    };
    use loom_daemon::role_runner::{RoleInvocationRunner, ScriptRoleInvocationRunner};
    let mut runner = ScriptRoleInvocationRunner::new(root.into());
    let result = runner.invoke("curator", "/loom:curator");
    assert!(result.is_success(), "{result:?}");
    assert_eq!(runner.resolved_model_effort().unwrap().0, "");
}
#[test]
fn native_role_runner_does_not_require_claude_tokens_or_inherit_sonnet() {
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "worker");
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("curator.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
    config(d.path(), serde_json::json!({"runtimes":{"default":"pi"}}));
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "role_runner_probe", "--nocapture"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("LOOM_TEST_NATIVE_ROLE_ROOT", d.path())
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_TOKENS_DIR", "")
        .env("HOME", d.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn opencode_pins_actual_working_directory_even_with_stale_pwd() {
    let d = tempfile::tempdir().unwrap();
    let out = worker(d.path(), "opencode")
        .env("PWD", "/stale-parent-checkout")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("arg=\"--dir\""), "{text}");
    assert!(
        text.contains(&format!("arg={:?}", d.path().canonicalize().unwrap().to_str().unwrap())),
        "{text}"
    );
    // 1.x keeps its whole shape: no 2.x-only flag, effort still a separate flag.
    assert!(!text.contains("arg=\"--standalone\""), "{text}");
    assert!(text.contains("arg=\"--variant\""), "{text}");
    assert!(!text.contains('#'), "{text}");
}

#[test]
fn opencode_argv_is_exact_per_major_version() {
    let d = tempfile::tempdir().unwrap();
    let cwd = d.path().canonicalize().unwrap();
    let cwd = cwd.to_str().unwrap();
    let launch = |version: &str, extra: &[&str]| {
        let out = worker(d.path(), "opencode")
            .env("PWD", "/stale-parent-checkout")
            .env("FIXTURE_VERSION", version)
            .env("FIXTURE_PRINT_ENV", "PWD")
            .args(extra)
            .args(["--dangerously-skip-permissions", "-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    // 1.x: byte-for-byte the argv this adapter produced before the probe existed.
    for version in [OPENCODE_V1, "v1.18.31", "opencode 1.18.31"] {
        let text = launch(version, &[]);
        assert_eq!(
            argv(&text),
            quoted(&[
                "run",
                "--format",
                "json",
                "--dir",
                cwd,
                "--model",
                "zai-coding-plan/glm-5.3-flash",
                "--variant",
                "max",
                "--auto",
                "--",
                "hello",
            ]),
            "{text}"
        );
    }
    // 2.x: no --dir, no --variant; a private server; effort rides on the model.
    let text = launch(OPENCODE_V2, &[]);
    assert_eq!(
        argv(&text),
        quoted(&[
            "run",
            "--format",
            "json",
            "--standalone",
            "--model",
            "zai-coding-plan/glm-5.3-flash#max",
            "--auto",
            "--",
            "hello",
        ]),
        "{text}"
    );
    // Without --dir, the working directory has to arrive by inheritance.
    assert!(text.contains(&format!("cwd={cwd:?}")), "{text}");
    assert!(text.contains(&format!("child_env PWD={cwd}")), "{text}");
    // No effort, no suffix.
    let text = launch(OPENCODE_V2, &["--model", "example/model-v2"]);
    assert_eq!(
        argv(&text),
        quoted(&[
            "run",
            "--format",
            "json",
            "--standalone",
            "--model",
            "example/model-v2",
            "--auto",
            "--",
            "hello"
        ]),
        "{text}"
    );
    // A '#' that would make the 2.x model#effort form ambiguous is refused.
    let out = worker(d.path(), "opencode")
        .env("FIXTURE_VERSION", OPENCODE_V2)
        .args([
            "--model",
            "example/model#v2",
            "--effort",
            "high",
            "-p",
            "hello",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous"));
    // ...and only there: 1.x passes effort as its own flag, so it is unaffected.
    let out = worker(d.path(), "opencode")
        .args([
            "--model",
            "example/model#v2",
            "--effort",
            "high",
            "-p",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn opencode_v2_unguarded_provider_block_still_travels_in_the_per_launch_config() {
    // After #8421 a profile's provider block reaches OpenCode ONLY through the
    // child's OPENCODE_CONFIG_CONTENT, which a shared 2.x background service never
    // sees: --standalone must therefore be present on an UNGUARDED launch too.
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"openweights","modelProfiles":{"openweights":{"model":"m","providers":{"opencode":"loom-openweights"},"credentialEnv":["LOOM_TEST_OPENWEIGHTS_KEY"],"credentialTargets":{"opencode":{"LOOM_TEST_OPENWEIGHTS_KEY":"LOOM_TEST_OPENWEIGHTS_KEY"}},"providerDefinition":{"opencode":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"https://example.invalid/v1","apiKey":"{env:LOOM_TEST_OPENWEIGHTS_KEY}"},"models":{"m":{}}}}}}}}),
    );
    let out = worker(d.path(), "opencode")
        .env("FIXTURE_VERSION", OPENCODE_V2)
        .env("LOOM_TEST_OPENWEIGHTS_KEY", "fake-openweights-secret-key")
        .env("FIXTURE_NATIVE_CONFIG", "1")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(argv(&text).contains(&"\"--standalone\"".to_string()), "{text}");
    assert!(!argv(&text).contains(&"\"--agent\"".to_string()), "{text}");
    let config = native_config(&text);
    assert_eq!(
        config["provider"]["loom-openweights"]["options"]["apiKey"],
        "{env:LOOM_TEST_OPENWEIGHTS_KEY}"
    );
    assert!(!text.contains("fake-openweights-secret-key"), "{text}");
}

#[test]
fn guarded_opencode_launch_is_refused_on_a_major_without_a_live_canary_receipt() {
    // SAFETY-CRITICAL (#8438). 2.x `run --auto` approves anything not explicitly
    // denied, and whether 2.x honors Loom's deny-by-default agent is unverified.
    // This fixture can only show that Loom REFUSES; it can never show that a real
    // 2.x CLI honors the guard. That is the out-of-band canary's job.
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    std::fs::write(d.path().join(".loom/roles/builder.md"), "Role task: $ARGUMENTS").unwrap();
    let guarded: [(&str, Option<&str>); 2] =
        [("hello", Some("builder")), ("/loom:builder 8438", None)];
    for (prompt, role) in guarded {
        let mut command = worker(d.path(), "opencode");
        if let Some(role) = role {
            command.env("LOOM_ROLE", role);
        }
        let out = command
            .env("FIXTURE_VERSION", OPENCODE_V2)
            .args(["--dangerously-skip-permissions", "-p", prompt])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(out.stdout.is_empty(), "the harness must never have started");
        assert!(stderr.contains("guarded"), "{stderr}");
        assert!(stderr.contains("v2.0.10"), "{stderr}");
        assert!(stderr.contains("1.x"), "{stderr}");
        assert!(stderr.contains("guardrail-parity-native.md"), "{stderr}");
        assert!(!stderr.contains("LOOM_CLI_START"), "{stderr}");
        // Refused before anything was provisioned, not after.
        assert!(!d.path().join(".loom/native-tools").exists());
    }
    // The same major, unguarded, is unaffected.
    let out = worker(d.path(), "opencode")
        .env("FIXTURE_VERSION", OPENCODE_V2)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("arg=\"--standalone\""));
    // Pi never probes OpenCode, so its guarded launches are untouched.
    let out = worker(d.path(), "pi")
        .env("LOOM_ROLE", "builder")
        .env("FIXTURE_VERSION", OPENCODE_V2)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Control: the identical guarded launch on the verified major still runs.
    let out = worker(d.path(), "opencode")
        .env("LOOM_ROLE", "builder")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("arg=\"loom-worker\""), "{text}");
    assert!(text.contains("arg=\"--dir\""), "{text}");
}

#[test]
fn unrecognized_opencode_version_is_refused_before_launch_and_missing_binary_is_127() {
    let d = tempfile::tempdir().unwrap();
    for (version, exit) in [
        ("3.0.0", "0"),
        ("opencode v3.1.4", "0"),
        ("0.9.9", "0"),
        ("garbage", "0"),
        ("", "0"),
        (OPENCODE_V1, "1"),
        (OPENCODE_V2, "70"),
    ] {
        let out = worker(d.path(), "opencode")
            .env("FIXTURE_VERSION", version)
            .env("FIXTURE_VERSION_EXIT", exit)
            .args(["-p", "hello"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{version:?}: {stderr}");
        assert!(stderr.contains("1.x") && stderr.contains("2.x"), "{version:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{version:?}");
    }
    let out = worker(d.path(), "opencode")
        .env("LOOM_OPENCODE_BIN", d.path().join("missing"))
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    // The probe belongs to the OpenCode arm only.
    let out = worker(d.path(), "pi")
        .env("FIXTURE_VERSION", "garbage")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn native_launches_null_stdin_even_when_the_parent_has_an_open_pipe() {
    // On OpenCode 2.x an open non-TTY stdin stalls `run` forever, silently. The
    // parent MUST hand the worker a live pipe here: `Command::output()` does not
    // inherit stdin, so without it this would pass even if the adapter stopped
    // nulling stdin. The legacy runtime below is the control that proves it.
    use std::process::Stdio;
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "claude");
    let run = |runtime: &str, version: &str| {
        let mut child = worker(d.path(), runtime)
            .env("FIXTURE_VERSION", version)
            .args(["-p", "hello"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Held open across the whole run: `wait_with_output` would close it first.
        let write_end = child.stdin.take().unwrap();
        let out = child.wait_with_output().unwrap();
        drop(write_end);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    for (runtime, version) in [
        ("pi", OPENCODE_V1),
        ("opencode", OPENCODE_V1),
        ("opencode", OPENCODE_V2),
    ] {
        let text = run(runtime, version);
        assert!(text.contains("stdin_is_dev_null=true"), "{runtime} {version}: {text}");
    }
    let text = run("claude", OPENCODE_V1);
    assert!(text.contains("stdin_is_dev_null=false"), "control must see the pipe: {text}");
}

#[test]
fn per_role_binding_is_shared_with_native_dispatch() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("curator.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"default":"claude","roles":{"curator":"opencode"}}}),
    );
    let out = worker(d.path(), "claude")
        .env_remove("LOOM_RUNTIME")
        .env("LOOM_ROLE", "curator")
        .env_remove("LOOM_RUNTIME_CURATOR")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("runtime=opencode"));
    let out = worker(d.path(), "opencode")
        .env("LOOM_ROLE", "curator")
        .env("LOOM_RUNTIME_CURATOR", "pi")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("runtime=pi"));
}

#[test]
fn credential_aliases_are_profile_data_and_never_argv_or_logs() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"test","modelProfiles":{"test":{"model":"m","providers":{"pi":"p","opencode":"p"},"credentialEnv":"LOOM_TEST_PROVIDER_SECRET","credentialTargets":{"pi":"LOOM_TEST_HARNESS_SECRET","opencode":"LOOM_TEST_HARNESS_SECRET"}}}}}),
    );
    for runtime in ["pi", "opencode"] {
        let out = worker(d.path(), runtime)
            .env("LOOM_TEST_PROVIDER_SECRET", "fake-test-secret")
            .args(["-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("credential_alias_matches=true"));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("fake-test-secret"));
        assert!(!String::from_utf8_lossy(&out.stderr).contains("fake-test-secret"));
    }
}

fn builder_role(root: &std::path::Path) {
    let roles = root.join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("builder.json"), "{}").unwrap();
}
fn native_config(text: &str) -> serde_json::Value {
    serde_json::from_str(
        text.lines()
            .find_map(|s| s.strip_prefix("native_config="))
            .unwrap(),
    )
    .unwrap()
}
fn profile_check(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["worker", "profile-check"])
        .args(args)
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env_remove("AWS_PROFILE")
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .env_remove("GOOGLE_CLOUD_PROJECT")
        .env_remove("VERTEX_LOCATION");
    c.output().unwrap()
}

#[test]
fn credential_env_accepts_the_legacy_string_form_and_a_required_array() {
    let d = tempfile::tempdir().unwrap();
    // String form: unchanged, and still optional so a CLI login keeps working.
    let out = worker(d.path(), "pi")
        .env_remove("ZAI_API_KEY")
        .args(["--profile", "zai-flash", "-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("glm-5.3-flash"));
    // Array form: every declared variable is mapped onto the harness's names.
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"multi","modelProfiles":{"multi":{"model":"m","providers":{"pi":"p"},"credentialEnv":["LOOM_TEST_ONE","LOOM_TEST_TWO"],"credentialTargets":{"pi":{"LOOM_TEST_ONE":"HARNESS_ONE","LOOM_TEST_TWO":"HARNESS_TWO"}}}}}}),
    );
    let out = worker(d.path(), "pi")
        .env("LOOM_TEST_ONE", "first-value")
        .env("LOOM_TEST_TWO", "second-value")
        .env("FIXTURE_PRINT_ENV", "HARNESS_ONE,HARNESS_TWO")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("child_env HARNESS_ONE=first-value"), "{text}");
    assert!(text.contains("child_env HARNESS_TWO=second-value"), "{text}");
    // The bundled examples are part of the schema surface: all four must load.
    for name in [
        "zai-flash",
        "example-bedrock",
        "example-vertex",
        "example-openai-compatible",
    ] {
        let out = profile_check(d.path(), &[name]);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains(&format!("profile: {name}")),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn bedrock_profile_injects_provider_options_and_maps_every_variable() {
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"bedrock","modelProfiles":{"bedrock":{"model":"m","providers":{"opencode":"amazon-bedrock"},"credentialEnv":["AWS_PROFILE","LOOM_TEST_BEDROCK_TOKEN"],"credentialTargets":{"opencode":{"AWS_PROFILE":"AWS_PROFILE","LOOM_TEST_BEDROCK_TOKEN":"AWS_BEARER_TOKEN_BEDROCK"}},"providerOptions":{"opencode":{"region":"us-east-1"}}}}}}),
    );
    let log = d.path().join("worker.log");
    let out = worker(d.path(), "opencode")
        .env("LOOM_ROLE", "builder")
        .env("AWS_PROFILE", "loom-trial")
        .env("LOOM_TEST_BEDROCK_TOKEN", "fake-bedrock-bearer-token")
        .env("FIXTURE_NATIVE_CONFIG", "1")
        .env("FIXTURE_PRINT_ENV", "AWS_PROFILE,AWS_BEARER_TOKEN_BEDROCK")
        .args(["--log", log.to_str().unwrap(), "-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // `--log` merges the launch record and the child's own streams into one file.
    let text = std::fs::read_to_string(&log).unwrap();
    let config = native_config(&text);
    assert_eq!(config["provider"]["amazon-bedrock"]["options"]["region"], "us-east-1");
    assert!(text.contains("child_env AWS_PROFILE=loom-trial"), "{text}");
    assert!(
        text.contains("child_env AWS_BEARER_TOKEN_BEDROCK=fake-bedrock-bearer-token"),
        "{text}"
    );
    // The launch record names harness/provider/model/effort, never a credential.
    let record = text.lines().find(|l| l.contains("LOOM_LAUNCH")).unwrap();
    for secret in ["fake-bedrock-bearer-token", "loom-trial"] {
        assert!(!record.contains(secret), "{record}");
        assert!(!config.to_string().contains(secret), "{config}");
    }
}

#[test]
fn custom_provider_definition_keeps_the_key_in_env_indirection_form() {
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"openweights","modelProfiles":{"openweights":{"model":"m","providers":{"opencode":"loom-openweights"},"credentialEnv":["LOOM_TEST_OPENWEIGHTS_KEY"],"credentialTargets":{"opencode":{"LOOM_TEST_OPENWEIGHTS_KEY":"LOOM_TEST_OPENWEIGHTS_KEY"}},"providerDefinition":{"opencode":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"https://example.invalid/v1","apiKey":"{env:LOOM_TEST_OPENWEIGHTS_KEY}"},"models":{"m":{}}}}}}}}),
    );
    for guarded in [true, false] {
        let mut command = worker(d.path(), "opencode");
        if guarded {
            command.env("LOOM_ROLE", "builder");
        }
        let out = command
            .env("LOOM_TEST_OPENWEIGHTS_KEY", "fake-openweights-secret-key")
            .env("FIXTURE_NATIVE_CONFIG", "1")
            .args(["-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        let injected = text
            .lines()
            .find(|s| s.starts_with("native_config="))
            .unwrap();
        let config = native_config(&text);
        let provider = &config["provider"]["loom-openweights"];
        assert_eq!(provider["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(provider["options"]["apiKey"], "{env:LOOM_TEST_OPENWEIGHTS_KEY}");
        assert_eq!(provider["options"]["baseURL"], "https://example.invalid/v1");
        assert!(provider["models"]["m"].is_object(), "{config}");
        // The exact injected string must never carry the literal key.
        assert!(!injected.contains("fake-openweights-secret-key"), "{injected}");
    }
}

#[test]
fn unset_or_embedded_credentials_fail_closed_before_launch() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"partial","modelProfiles":{"partial":{"model":"m","providers":{"opencode":"p"},"credentialEnv":["LOOM_TEST_PRESENT","LOOM_TEST_ABSENT_A","LOOM_TEST_ABSENT_B"],"credentialTargets":{"opencode":{"LOOM_TEST_PRESENT":"HARNESS_KEY"}}}}}}),
    );
    let out = worker(d.path(), "opencode")
        .env("LOOM_TEST_PRESENT", "fake-present-value")
        .env_remove("LOOM_TEST_ABSENT_A")
        .env_remove("LOOM_TEST_ABSENT_B")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("LOOM_TEST_ABSENT_A"), "{text}");
    assert!(text.contains("LOOM_TEST_ABSENT_B"), "{text}");
    assert!(!text.contains("fake-present-value"), "{text}");
    assert!(out.stdout.is_empty());
    // A credential VALUE pasted into provider configuration is also fail-closed.
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"leaky","modelProfiles":{"leaky":{"model":"m","providers":{"opencode":"p"},"credentialEnv":["LOOM_TEST_PRESENT"],"providerOptions":{"opencode":{"apiKey":"fake-present-value"}}}}}}),
    );
    let out = worker(d.path(), "opencode")
        .env("LOOM_TEST_PRESENT", "fake-present-value")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&out.stderr).contains("{env:LOOM_TEST_PRESENT}"));
}

#[test]
fn profile_check_reports_resolvability_without_spawning() {
    let d = tempfile::tempdir().unwrap();
    let out = profile_check(d.path(), &["example-bedrock"]);
    assert_eq!(out.status.code(), Some(78));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("harness opencode: provider amazon-bedrock"), "{text}");
    assert!(text.contains("providerOptions: region"), "{text}");
    assert!(text.contains("unresolvable, set AWS_PROFILE"), "{text}");
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    let out = c
        .args(["worker", "profile-check", "example-bedrock"])
        .current_dir(d.path())
        .env("LOOM_WORKSPACE", d.path())
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("AWS_PROFILE", "loom-trial")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stdout).contains("status: resolvable"));
    // The lenient string form stays resolvable without the variable exported.
    let out = profile_check(d.path(), &["zai-flash"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stdout).contains("harness pi: provider zai"));
    let out = profile_check(d.path(), &["example-vertex", "--runtime", "pi"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no provider binding"));
    let out = profile_check(d.path(), &["no-such-profile"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown model profile"));
}

#[test]
fn malformed_profile_selection_does_not_fall_back_to_a_billable_default() {
    let d = tempfile::tempdir().unwrap();
    for value in [
        serde_json::json!({"defaultModelProfile":12}),
        serde_json::json!({"modelProfiles":[]}),
        serde_json::json!({"defaultModelProfile":"unknown"}),
    ] {
        config(d.path(), serde_json::json!({"runtimes":value}));
        let out = worker(d.path(), "pi")
            .args(["-p", "hello"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(78));
        assert!(out.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn legacy_argv_preserves_non_utf8_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "claude");
    let arg = std::ffi::OsString::from_vec(b"filename-\xff".to_vec());
    let out = worker(d.path(), "claude").arg(&arg).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains(&format!("arg={arg:?}")));
}

#[test]
fn native_sweep_prompt_uses_sequential_cli_lifecycle_and_fails_closed_tools() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(
        roles.join("builder.json"),
        r#"{"runtimeRequirements":["loomControl","worktreeIsolation"]}"#,
    )
    .unwrap();
    for role in ["curator", "judge", "doctor"] {
        std::fs::write(
            roles.join(format!("{role}.json")),
            r#"{"runtimeRequirements":["loomControl"]}"#,
        )
        .unwrap();
    }
    for runtime in ["pi", "opencode"] {
        let out = worker(d.path(), runtime)
            .args(["-p", "/loom:sweep 8399 --claim-owned 8399"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("8399 --claim-owned 8399"));
        assert!(text.contains("Native sequential issue lifecycle"));
        assert!(text.contains("post-verdict.sh"));
        assert!(text.contains("merge-pr.sh"));
        if runtime == "pi" {
            assert!(text.contains("--no-builtin-tools"));
        } else {
            assert!(text.contains("loom-worker"));
        }
    }
}

#[test]
fn native_sweep_cannot_bypass_a_phase_capability_requirement() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    for role in ["builder", "curator", "doctor"] {
        std::fs::write(roles.join(format!("{role}.json")), "{}").unwrap();
    }
    std::fs::write(roles.join("judge.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    for runtime in ["pi", "opencode"] {
        let out = worker(d.path(), runtime)
            .args(["-p", "/loom:sweep 1"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(78));
        assert!(String::from_utf8_lossy(&out.stderr).contains("native sweep phase judge"));
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn guarded_opencode_pins_auxiliary_models_preserves_provider_and_worker_identity() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("builder.json"), "{}").unwrap();
    let out = worker(d.path(), "opencode")
        .env("LOOM_ROLE", "builder")
        .env(
            "OPENCODE_CONFIG_CONTENT",
            r#"{"small_model":"anthropic/stale","provider":{"private":{"name":"fixture"}}}"#,
        )
        .env("FIXTURE_NATIVE_CONFIG", "1")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    let config: serde_json::Value = serde_json::from_str(
        text.lines()
            .find_map(|s| s.strip_prefix("native_config="))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(config["small_model"], "zai-coding-plan/glm-5.3-flash");
    assert_eq!(config["agent"]["loom-worker"]["model"], config["small_model"]);
    assert_eq!(config["provider"]["private"]["name"], "fixture");
    assert_eq!(config["permission"]["*"], "deny");
    assert!(text.contains("native_worker_pid_matches=true"));
}

/// #8448: a guarded launch whose native JSON stream never actually used a
/// `loom_*` tool must be classified as a failed launch, independent of exit
/// code — exercised through the real `spawn-worker` seam via the fake-CLI
/// fixture, not just a unit test of the classifier in isolation. The
/// role-runner side of the same verdict (exit-0 `Success` demoted to
/// `Failure`) is covered by `role_runner::toolless_launch::tests`.
#[test]
fn a_toolless_guarded_completion_is_classified_as_a_failed_launch() {
    use loom_daemon::worker_spawn::launch_outcome::classify_native_stream;
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("builder.json"), "{}").unwrap();
    let run = |stream: &str| {
        let log = d.path().join(format!("{stream}.log"));
        let out = worker(d.path(), "pi")
            .env("LOOM_ROLE", "builder")
            .env("FIXTURE_NATIVE_STREAM", stream)
            .args(["--log", log.to_str().unwrap(), "-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        std::fs::read_to_string(log).unwrap()
    };
    // The exact receipt: exit 0, yet the classifier must still call it failed
    // — and it must be the ACTIONABLE form of the verdict (the stream was
    // read: `events > 0`), not the trivially-true "no opinion" one.
    let toolless = classify_native_stream(&run("toolless"));
    assert_eq!(toolless.loom_tool_uses, 0);
    assert_eq!(toolless.step_finishes, 0);
    assert!(toolless.observed_a_toolless_run());
    // Control: an otherwise-identical launch that DID use a loom_* tool is not.
    let used = classify_native_stream(&run("used"));
    assert_eq!(used.loom_tool_uses, 1);
    assert!(!used.observed_a_toolless_run());
}
