//! Regression tests for the native worker seam. No provider calls or shell fixtures.
use std::{path::PathBuf, process::Command, sync::OnceLock};
fn fixture() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let bin = dir.join("harness");
        assert!(Command::new("rustc")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/worker_cli.rs"))
            .arg("-o")
            .arg(&bin)
            .status()
            .unwrap()
            .success());
        bin
    })
}
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
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture());
    c
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
