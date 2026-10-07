//! LLM-gateway routing for spawned harnesses (issue #9473), driven through
//! the real `spawn-worker` / `worker profile-check` paths with the shared fake
//! harness: what a routed native harness receives, that an unrouted profile is
//! untouched, and that Claude and Codex never receive any of it.
//!
//! A sibling of `worker_spawn.rs` rather than a section of it: that file sits
//! at the 1000-line file-size ratchet. `worker`, `config` and `builder_role`
//! below are deliberate copies of its own.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;

const URL: &str = "http://llm-gateway.example.net:8080/v1";
const VK: &str = "sk-bf-fixture0123456789abcdef0123";
const REAL_KEY: &str = "fixture-real-provider-key-never-forwarded";
const GATEWAY_ENV: [&str; 5] = [
    "LOOM_LLM_GATEWAY_URL",
    "LOOM_LLM_GATEWAY_PROFILES",
    "LOOM_LLM_GATEWAY_VK",
    "LOOM_LLM_GATEWAY_VK_FILE",
    "LOOM_LLM_GATEWAY_VK_HEADER",
];

fn worker(root: &Path, runtime: &str) -> Command {
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
        .env("FIXTURE_VERSION", "1.18.31")
        .env_remove("FIXTURE_VERSION_EXIT")
        .env_remove("CEREBRAS_API_KEY")
        .env_remove("ZAI_API_KEY")
        .env_remove("ZHIPU_API_KEY")
        // #9964: the codex launch resolves its profile root. An empty override
        // disables it, so the binary never reads the host's real
        // `~/.loom/codex-profiles`, and the pins are cleared so a developer
        // shell's `CODEX_HOME` cannot take a different path than CI does.
        .env("LOOM_CODEX_PROFILE_ROOT", "")
        .env_remove("CODEX_HOME")
        .env_remove("LOOM_CODEX_HOME")
        .env_remove("LOOM_CODEX_PROFILE")
        .env_remove("LOOM_CODEX_NO_EXEC")
        .env_remove("LOOM_SPAWN_NO_EXPORT");
    // A developer shell exporting the contract must not leak into a fixture.
    for name in GATEWAY_ENV {
        c.env_remove(name);
    }
    c
}

fn config(root: &Path, value: serde_json::Value) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), value.to_string()).unwrap();
}

fn builder_role(root: &Path) {
    let roles = root.join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("builder.json"), "{}").unwrap();
}

/// The virtual key in an owner-only file, the recommended source.
fn vk_file(root: &Path, mode: u32) -> PathBuf {
    let path = root.join("gateway.vk");
    std::fs::write(&path, format!("LOOM_LLM_GATEWAY_VK={VK}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let _ = mode;
    path
}

fn route(command: &mut Command, profiles: &str, vk: &Path) -> Output {
    command
        .env("LOOM_LLM_GATEWAY_URL", URL)
        .env("LOOM_LLM_GATEWAY_PROFILES", profiles)
        .env("LOOM_LLM_GATEWAY_VK_FILE", vk)
        .args(["-p", "hello"])
        .output()
        .unwrap()
}

fn print_env(extra: &[&str]) -> String {
    GATEWAY_ENV
        .iter()
        .chain(extra)
        .copied()
        .collect::<Vec<_>>()
        .join(",")
}

/// The fake harness's `child_env NAME=value` line for `name`.
fn child_env<'a>(stdout: &'a str, name: &str) -> &'a str {
    let prefix = format!("child_env {name}=");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no {name} line in {stdout}"))
}

fn launch_record(stderr: &str) -> serde_json::Value {
    let line = stderr
        .lines()
        .find_map(|l| l.strip_prefix("# LOOM_LAUNCH "))
        .unwrap_or_else(|| panic!("no launch record in {stderr}"));
    serde_json::from_str(line).unwrap()
}

fn texts(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_routed_pi_profile_reaches_the_gateway_with_the_virtual_key_only() {
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    let vk = vk_file(d.path(), 0o600);
    let out = route(
        worker(d.path(), "pi")
            .env("LOOM_ROLE", "builder")
            .env("LOOM_MODEL_PROFILE", "quick-cerebras")
            .env("CEREBRAS_API_KEY", REAL_KEY)
            .env("FIXTURE_PRINT_PI_MODELS", "1")
            .env("FIXTURE_PRINT_ENV", print_env(&["CEREBRAS_API_KEY"])),
        "quick-cerebras",
        &vk,
    );
    let (stdout, stderr) = texts(&out);
    assert!(out.status.success(), "{stderr}");
    // The key replaces the provider key under the harness's own variable.
    assert_eq!(child_env(&stdout, "CEREBRAS_API_KEY"), VK);
    // The raw contract never crosses into the harness.
    for name in GATEWAY_ENV {
        assert_eq!(child_env(&stdout, name), "", "{name} leaked: {stdout}");
    }
    let models: serde_json::Value = serde_json::from_str(
        stdout
            .lines()
            .find_map(|l| l.strip_prefix("pi_models="))
            .unwrap(),
    )
    .unwrap();
    let provider = &models["providers"]["cerebras"];
    assert_eq!(provider["baseUrl"], URL);
    assert_eq!(provider["apiKey"], "${CEREBRAS_API_KEY}");
    assert_eq!(provider["headers"]["x-bf-vk"], "${CEREBRAS_API_KEY}");
    let record = launch_record(&stderr);
    assert_eq!(record["credentialSource"], "gateway");
    assert_eq!(record["profile"], "quick-cerebras");
    assert!(
        stderr.contains("# LOOM_LLM_GATEWAY runtime=pi profile=quick-cerebras"),
        "{stderr}"
    );
    // Neither the real key nor the virtual key is ever logged.
    assert!(!stdout.contains(REAL_KEY) && !stderr.contains(REAL_KEY), "{stdout}{stderr}");
    assert!(!stderr.contains(VK), "{stderr}");
}

#[test]
fn a_routed_opencode_profile_gets_base_url_key_and_header_guarded_or_not() {
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    let vk = vk_file(d.path(), 0o600);
    for guarded in [true, false] {
        let mut command = worker(d.path(), "opencode");
        if guarded {
            command.env("LOOM_ROLE", "builder");
        }
        // The bundled default profile `zai-flash`: ZAI_API_KEY -> ZHIPU_API_KEY.
        let out = route(
            command
                .env("ZAI_API_KEY", REAL_KEY)
                .env("FIXTURE_NATIVE_CONFIG", "1")
                .env("FIXTURE_PRINT_ENV", print_env(&["ZHIPU_API_KEY", "ZAI_API_KEY"])),
            "zai-flash",
            &vk,
        );
        let (stdout, stderr) = texts(&out);
        assert!(out.status.success(), "guarded={guarded}: {stderr}");
        let injected: serde_json::Value = serde_json::from_str(
            stdout
                .lines()
                .find_map(|l| l.strip_prefix("native_config="))
                .unwrap(),
        )
        .unwrap();
        let options = &injected["provider"]["zai-coding-plan"]["options"];
        assert_eq!(options["baseURL"], URL, "{injected}");
        assert_eq!(options["apiKey"], "{env:ZHIPU_API_KEY}");
        assert_eq!(options["headers"]["x-bf-vk"], "{env:ZHIPU_API_KEY}");
        assert_eq!(child_env(&stdout, "ZHIPU_API_KEY"), VK);
        // The provider's own source variable is dropped: the gateway holds it.
        assert_eq!(child_env(&stdout, "ZAI_API_KEY"), "");
        for name in GATEWAY_ENV {
            assert_eq!(child_env(&stdout, name), "", "{name} leaked: {stdout}");
        }
        assert!(!injected.to_string().contains(VK), "{injected}");
        assert_eq!(launch_record(&stderr)["credentialSource"], "gateway");
    }
}

/// The red line (2am D39): with every knob saying "route" — raw key in the
/// environment, the profile opted in by env and by config — a Claude or Codex
/// launch receives none of the contract and no base-URL override.
#[test]
fn claude_and_codex_never_receive_the_gateway() {
    let d = tempfile::tempdir().unwrap();
    let scripts = d.path().join(".loom/scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    for runtime in ["claude", "codex"] {
        std::fs::copy(fixture(), scripts.join(format!("spawn-{runtime}.sh"))).unwrap();
    }
    config(
        d.path(),
        serde_json::json!({"runtimes":{"llmGateway":{"url":URL,"profiles":["quick-cerebras","zai-flash"]}}}),
    );
    let vk = vk_file(d.path(), 0o600);
    let overrides = [
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_API_KEY",
    ];
    for runtime in ["claude", "codex"] {
        let mut command = worker(d.path(), runtime);
        for name in overrides {
            command.env_remove(name);
        }
        let out = route(
            command
                .env("LOOM_LLM_GATEWAY_VK", VK)
                .env("LOOM_MODEL_PROFILE", "quick-cerebras")
                .env("FIXTURE_PRINT_ENV", print_env(&overrides)),
            "quick-cerebras zai-flash",
            &vk,
        );
        let (stdout, stderr) = texts(&out);
        assert!(out.status.success(), "{runtime}: {stderr}");
        assert!(stdout.contains(&format!("LOOM_RUNTIME={runtime}")), "{stdout}");
        for name in GATEWAY_ENV.iter().chain(&overrides) {
            assert_eq!(child_env(&stdout, name), "", "{runtime} received {name}");
        }
        assert!(!stdout.contains(VK) && !stderr.contains(VK), "{runtime}: {stdout}{stderr}");
        assert!(!stderr.contains("LOOM_LLM_GATEWAY runtime="), "{stderr}");
    }
}

#[test]
fn an_unrouted_profile_and_the_off_switch_launch_exactly_as_before() {
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    let vk = vk_file(d.path(), 0o600);
    config(
        d.path(),
        serde_json::json!({"runtimes":{"llmGateway":{"url":URL,"profiles":["quick-cerebras"]}}}),
    );
    for (profiles, url) in [("zai-glm", None), ("quick-cerebras", Some("off"))] {
        let mut command = worker(d.path(), "pi");
        if let Some(url) = url {
            command.env("LOOM_LLM_GATEWAY_URL", url);
        }
        let out = command
            .env("LOOM_ROLE", "builder")
            .env("LOOM_MODEL_PROFILE", "quick-cerebras")
            .env("LOOM_LLM_GATEWAY_PROFILES", profiles)
            .env("LOOM_LLM_GATEWAY_VK_FILE", &vk)
            .env("CEREBRAS_API_KEY", REAL_KEY)
            .env("FIXTURE_PRINT_PI_MODELS", "1")
            .env("FIXTURE_PRINT_ENV", print_env(&["CEREBRAS_API_KEY"]))
            .args(["-p", "hello"])
            .output()
            .unwrap();
        let (stdout, stderr) = texts(&out);
        assert!(out.status.success(), "{stderr}");
        assert_eq!(child_env(&stdout, "CEREBRAS_API_KEY"), REAL_KEY);
        assert!(stdout.contains("pi_models=\n"), "no override expected: {stdout}");
        assert_eq!(launch_record(&stderr)["credentialSource"], "env");
        // Scrubbed all the same: an unrouted harness never sees the contract.
        for name in GATEWAY_ENV {
            assert_eq!(child_env(&stdout, name), "", "{name} leaked: {stdout}");
        }
    }
}

#[test]
fn a_routed_profile_that_cannot_be_routed_safely_refuses_at_78_without_secrets() {
    let d = tempfile::tempdir().unwrap();
    builder_role(d.path());
    let good = vk_file(d.path(), 0o600);
    config(
        d.path(),
        serde_json::json!({"runtimes":{"modelProfiles":{"login-store":{"model":"m","providers":{"opencode":"anthropic"}}}}}),
    );
    let cases: Vec<(&str, Command, &str)> = vec![
        // Opted in with no key configured anywhere.
        (
            "missing key",
            {
                let mut c = worker(d.path(), "pi");
                c.env("LOOM_ROLE", "builder")
                    .env("LOOM_MODEL_PROFILE", "quick-cerebras")
                    .env("LOOM_LLM_GATEWAY_URL", URL)
                    .env("LOOM_LLM_GATEWAY_PROFILES", "quick-cerebras")
                    .args(["-p", "hello"]);
                c
            },
            "LOOM_LLM_GATEWAY_VK_FILE",
        ),
        // Pi's override needs Loom's per-launch agent directory.
        (
            "unguarded pi",
            {
                let mut c = worker(d.path(), "pi");
                c.env("LOOM_MODEL_PROFILE", "quick-cerebras")
                    .env("LOOM_LLM_GATEWAY_URL", URL)
                    .env("LOOM_LLM_GATEWAY_PROFILES", "quick-cerebras")
                    .env("LOOM_LLM_GATEWAY_VK_FILE", &good)
                    .args(["-p", "hello"]);
                c
            },
            "unguarded",
        ),
        // A harness-login (subscription) profile is never routed.
        (
            "login store",
            {
                let mut c = worker(d.path(), "opencode");
                c.env("LOOM_MODEL_PROFILE", "login-store")
                    .env("LOOM_LLM_GATEWAY_URL", URL)
                    .env("LOOM_LLM_GATEWAY_PROFILES", "login-store")
                    .env("LOOM_LLM_GATEWAY_VK_FILE", &good)
                    .args(["-p", "hello"]);
                c
            },
            "exactly one API-key variable",
        ),
        // A Claude credential presented as the virtual key.
        (
            "anthropic key",
            {
                let mut c = worker(d.path(), "opencode");
                c.env("LOOM_LLM_GATEWAY_URL", URL)
                    .env("LOOM_LLM_GATEWAY_PROFILES", "zai-flash")
                    .env("LOOM_LLM_GATEWAY_VK", "sk-ant-oat01-fixture-subscription-token")
                    .args(["-p", "hello"]);
                c
            },
            "never traverses the gateway",
        ),
    ];
    for (name, mut command, expected) in cases {
        let out = command.output().unwrap();
        let (stdout, stderr) = texts(&out);
        assert_eq!(out.status.code(), Some(78), "{name}: {stdout}{stderr}");
        assert!(stderr.contains(expected), "{name}: {stderr}");
        assert!(stdout.is_empty(), "{name}: the harness must not start: {stdout}");
        for secret in [VK, "sk-ant-oat01-fixture-subscription-token"] {
            assert!(!stderr.contains(secret), "{name}: {stderr}");
        }
    }
    #[cfg(unix)]
    {
        let loose = vk_file(d.path(), 0o644);
        let out =
            route(worker(d.path(), "opencode").env("LOOM_ROLE", "builder"), "zai-flash", &loose);
        let (_, stderr) = texts(&out);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(stderr.contains("chmod 600") && !stderr.contains(VK), "{stderr}");
    }
}

#[test]
fn profile_check_reports_the_route_without_reading_the_key() {
    let d = tempfile::tempdir().unwrap();
    let vk = vk_file(d.path(), 0o600);
    let check = |vk: &Path| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        for name in GATEWAY_ENV {
            c.env_remove(name);
        }
        let out = c
            .args([
                "worker",
                "profile-check",
                "quick-cerebras",
                "--runtime",
                "pi",
            ])
            .current_dir(d.path())
            .env("LOOM_WORKSPACE", d.path())
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_SHARED_API_KEYS_DIR", "")
            .env_remove("CEREBRAS_API_KEY")
            .env("LOOM_LLM_GATEWAY_URL", URL)
            .env("LOOM_LLM_GATEWAY_PROFILES", "quick-cerebras")
            .env("LOOM_LLM_GATEWAY_VK_FILE", vk)
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let (ok, text) = check(&vk);
    assert!(ok, "{text}");
    assert!(
        text.contains(&format!(
            "llm gateway: routed, url={URL} credential=CEREBRAS_API_KEY vk=file header=x-bf-vk"
        )),
        "{text}"
    );
    assert!(text.contains("(supplied by the LLM gateway virtual key)"), "{text}");
    assert!(text.contains("status: resolvable"), "{text}");
    let (ok, text) = check(&d.path().join("missing.vk"));
    assert!(!ok, "{text}");
    assert!(text.contains("status: unresolvable, spawn would refuse at 78"), "{text}");
    assert!(!text.contains(VK), "{text}");
}
