//! The #9473 env contract, the per-profile allowlist and the Claude /
//! subscription exclusion. Settings are read through an injected environment
//! (`plan_with` / `open_with`), so nothing here mutates the process env.
use super::*;
use std::collections::BTreeMap;

const URL: &str = "http://llm-gateway.example.net:8080/v1";
const VK: &str = "sk-bf-0123456789abcdef0123456789abcdef";

/// A fake environment: `pairs` set, everything else unset.
fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |key: &str| map.get(key).cloned().filter(|v| !v.trim().is_empty())
}

/// The bundled `quick-cerebras` shape on Pi: one API key, implicit target.
fn api_key_selection(profile: &str) -> Selection {
    Selection {
        provider: "cerebras".into(),
        model: "gpt-oss-120b".into(),
        effort: None,
        profile: Some(profile.into()),
        credentials: vec![("CEREBRAS_API_KEY".into(), "CEREBRAS_API_KEY".into())],
        credential_sources: vec!["CEREBRAS_API_KEY".into()],
        provider_options: None,
        provider_definition: None,
        credential_pool: Some("cerebras".into()),
        credential_proxy: None,
    }
}

/// The 2am `zai-glm` shape on OpenCode: source and target differ.
fn zai_opencode() -> Selection {
    Selection {
        provider: "zai-coding-plan".into(),
        model: "glm-5.3".into(),
        credentials: vec![("ZAI_API_KEY".into(), "ZHIPU_API_KEY".into())],
        credential_sources: vec!["ZAI_API_KEY".into()],
        credential_pool: None,
        ..api_key_selection("zai-glm")
    }
}

fn routed_env() -> impl Fn(&str) -> Option<String> {
    env_of(&[
        (URL_ENV, URL),
        (PROFILES_ENV, "quick-cerebras, zai-glm"),
        (VK_ENV, VK),
    ])
}

fn plan_of(runtime: &str, selection: &Selection, env: Env) -> Result<Option<Plan>, LaunchError> {
    plan_with(runtime, selection, &json!({}), env)
}

// ---- the env contract ------------------------------------------------------

#[test]
fn an_opted_in_api_key_profile_is_routed_with_the_default_header() {
    let plan = plan_of("pi", &api_key_selection("quick-cerebras"), &routed_env())
        .unwrap()
        .expect("routed");
    assert_eq!(plan.url, URL);
    assert_eq!(plan.target, "CEREBRAS_API_KEY");
    assert_eq!(plan.header.as_deref(), Some(DEFAULT_VK_HEADER));
    assert_eq!(plan.vk, VkSource::Env);
}

#[test]
fn no_url_means_the_feature_is_off_whatever_else_is_set() {
    let env = env_of(&[(PROFILES_ENV, "quick-cerebras"), (VK_ENV, VK)]);
    assert!(plan_of("pi", &api_key_selection("quick-cerebras"), &env)
        .unwrap()
        .is_none());
}

#[test]
fn env_beats_config_and_off_is_the_break_glass() {
    let config = json!({"runtimes": {"llmGateway": {
        "url": "https://config.example.net/v1",
        "profiles": ["quick-cerebras"],
        "virtualKeyFile": "/nonexistent/config.vk",
        "virtualKeyHeader": "x-config-vk"
    }}});
    let selection = api_key_selection("quick-cerebras");
    // Config alone routes, with every config value.
    let plan = plan_with("pi", &selection, &config, &env_of(&[]))
        .unwrap()
        .unwrap();
    assert_eq!(plan.url, "https://config.example.net/v1");
    assert_eq!(plan.vk, VkSource::File("/nonexistent/config.vk".into()));
    assert_eq!(plan.header.as_deref(), Some("x-config-vk"));
    // Env beats config, field by field.
    let env = env_of(&[
        (URL_ENV, URL),
        (VK_FILE_ENV, "/nonexistent/env.vk"),
        (VK_HEADER_ENV, "none"),
    ]);
    let plan = plan_with("pi", &selection, &config, &env).unwrap().unwrap();
    assert_eq!(plan.url, URL);
    assert_eq!(plan.vk, VkSource::File("/nonexistent/env.vk".into()));
    assert_eq!(plan.header, None, "`none` sends the key as the bearer key only");
    // The value beats either file.
    let env = env_of(&[(VK_ENV, VK), (VK_FILE_ENV, "/nonexistent/env.vk")]);
    assert_eq!(
        plan_with("pi", &selection, &config, &env)
            .unwrap()
            .unwrap()
            .vk,
        VkSource::Env
    );
    // An env profile list replaces the config list rather than extending it.
    let env = env_of(&[(PROFILES_ENV, "zai-glm")]);
    assert!(plan_with("pi", &selection, &config, &env)
        .unwrap()
        .is_none());
    // `off` disables routing even with a complete config block.
    let env = env_of(&[(URL_ENV, "OFF")]);
    assert!(plan_with("pi", &selection, &config, &env)
        .unwrap()
        .is_none());
}

#[test]
fn the_virtual_key_never_belongs_in_configuration() {
    let config = json!({"runtimes": {"llmGateway": {"url": URL, "profiles": ["quick-cerebras"], "virtualKey": VK}}});
    let error =
        plan_with("pi", &api_key_selection("quick-cerebras"), &config, &env_of(&[])).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error.message.contains("never belongs in configuration"), "{}", error.message);
    assert!(!error.message.contains(VK), "{}", error.message);
}

#[test]
fn a_routed_profile_without_a_key_or_with_a_bad_url_or_header_refuses() {
    let selection = api_key_selection("quick-cerebras");
    let no_key = env_of(&[(URL_ENV, URL), (PROFILES_ENV, "quick-cerebras")]);
    let error = plan_of("pi", &selection, &no_key).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error.message.contains(VK_FILE_ENV), "{}", error.message);
    for url in [
        "https://user:pass@gw.example.net/v1",
        "gw.example.net/v1",
        "ftp://gw.example.net",
    ] {
        let env = env_of(&[
            (URL_ENV, url),
            (PROFILES_ENV, "quick-cerebras"),
            (VK_ENV, VK),
        ]);
        let error = plan_of("pi", &selection, &env).unwrap_err();
        assert!(error.message.contains("URL is unusable"), "{url}: {}", error.message);
        assert!(!error.message.contains("pass"), "{}", error.message);
    }
    for header in ["Authorization", "x bf vk", "x-bf-vk:"] {
        let env = env_of(&[
            (URL_ENV, URL),
            (PROFILES_ENV, "quick-cerebras"),
            (VK_ENV, VK),
            (VK_HEADER_ENV, header),
        ]);
        assert!(plan_of("pi", &selection, &env).is_err(), "{header}");
    }
}

// ---- the per-profile allowlist ---------------------------------------------

#[test]
fn only_profiles_named_in_the_allowlist_are_routed() {
    let env = routed_env();
    assert!(plan_of("pi", &api_key_selection("quick-flash"), &env)
        .unwrap()
        .is_none());
    assert!(plan_of("opencode", &zai_opencode(), &env)
        .unwrap()
        .is_some());
    // A launch with no profile (an explicit provider/model) is never routed.
    let mut bare = api_key_selection("quick-cerebras");
    bare.profile = None;
    assert!(plan_of("pi", &bare, &env).unwrap().is_none());
}

// ---- the Claude / subscription exclusion -----------------------------------

#[test]
fn claude_and_codex_are_never_routed_even_when_everything_says_route() {
    let env = env_of(&[
        (URL_ENV, URL),
        (PROFILES_ENV, "quick-cerebras zai-glm"),
        (VK_ENV, VK),
    ]);
    for runtime in NEVER_RUNTIMES {
        assert!(!runtime_eligible(runtime));
        for selection in [api_key_selection("quick-cerebras"), zai_opencode()] {
            assert!(plan_of(runtime, &selection, &env).unwrap().is_none(), "{runtime}");
        }
    }
    for runtime in ["gemini", "aider", "generic"] {
        assert!(!runtime_eligible(runtime), "{runtime}");
        // An opted-in profile on an unmapped harness refuses rather than
        // silently launching around the gateway.
        assert!(plan_of(runtime, &api_key_selection("quick-cerebras"), &env).is_err());
    }
    for runtime in MAPPED_RUNTIMES {
        assert!(runtime_eligible(runtime), "{runtime}");
    }
}

#[test]
fn a_subscription_or_login_store_profile_refuses_instead_of_routing() {
    let env = routed_env();
    // No credentialEnv at all: the harness's own login store (a Kimi or
    // Claude Pro subscription, an OpenCode `auth login`).
    let mut login = api_key_selection("quick-cerebras");
    login.credentials.clear();
    login.credential_sources.clear();
    let error = plan_of("opencode", &login, &env).unwrap_err();
    assert!(error.message.contains("exactly one API-key variable"), "{}", error.message);
    // A multi-variable provider (Bedrock, Vertex) is not one API key either.
    let mut bedrock = api_key_selection("quick-cerebras");
    bedrock.credential_sources.push("AWS_PROFILE".into());
    assert!(plan_of("opencode", &bedrock, &env).is_err());
    // An OAuth token mapped as if it were a key.
    let mut oauth = api_key_selection("quick-cerebras");
    oauth.credentials = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "ANTHROPIC_AUTH_TOKEN".into())];
    oauth.credential_sources = vec!["CLAUDE_CODE_OAUTH_TOKEN".into()];
    let error = plan_of("opencode", &oauth, &env).unwrap_err();
    assert!(error.message.contains("OAuth"), "{}", error.message);
    // credentialProxy re-points the same endpoint.
    let mut proxied = api_key_selection("quick-cerebras");
    proxied.credential_proxy = Some(crate::worker_spawn::egress_proxy::ProfileProxy {
        upstream: "https://api.example.com".into(),
        header: crate::worker_spawn::egress_proxy::HeaderStyle::AuthorizationBearer,
        base_url_env: vec![],
        observe: false,
    });
    assert!(plan_of("opencode", &proxied, &env).is_err());
}

#[test]
fn an_anthropic_credential_is_refused_as_the_virtual_key() {
    for value in [
        "sk-ant-oat01-abcdefghijklmnop",
        "sk-ant-api03-abcdefghijklmnop",
    ] {
        let env = env_of(&[
            (URL_ENV, URL),
            (PROFILES_ENV, "quick-cerebras"),
            (VK_ENV, value),
        ]);
        let plan = plan_of("pi", &api_key_selection("quick-cerebras"), &env)
            .unwrap()
            .unwrap();
        let error = plan.open_with(&env).unwrap_err();
        assert_eq!(error.code, 78);
        assert!(error.message.contains("never traverses the gateway"), "{}", error.message);
        assert!(!error.message.contains(value), "{}", error.message);
    }
}

#[test]
fn guard_dispatch_keeps_the_contract_only_for_spawn_worker_and_a_mapped_runtime() {
    let removed = |cmd: &Command| -> Vec<String> {
        cmd.get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect()
    };
    let seam = Path::new("/repo/.loom/scripts/spawn-worker.sh");
    for (bin, runtime, scrubbed) in [
        (seam, Some("opencode"), false),
        (seam, Some("pi"), false),
        // Unknown runtime: spawn-worker decides and scrubs legacy adapters.
        (seam, None, false),
        (seam, Some("claude"), true),
        (seam, Some("codex"), true),
        (seam, Some("gemini"), true),
        // A spawn bin that is not the seam never maps the contract.
        (Path::new("/repo/.loom/scripts/spawn-claude.sh"), Some("opencode"), true),
        (Path::new("/opt/custom-launcher"), None, true),
    ] {
        let mut cmd = Command::new("/bin/true");
        guard_dispatch(&mut cmd, bin, runtime);
        let mut got = removed(&cmd);
        got.sort();
        let mut want: Vec<String> = if scrubbed {
            ENV_NAMES.iter().map(|s| (*s).to_string()).collect()
        } else {
            Vec::new()
        };
        want.sort();
        assert_eq!(got, want, "{} {runtime:?}", bin.display());
    }
}

// ---- the virtual key -------------------------------------------------------

#[test]
fn the_key_file_accepts_a_bare_value_or_its_own_assignment_line() {
    for text in [
        "sk-bf-abc\n",
        "# vk-loom-fleet\n\nsk-bf-abc\n",
        "LOOM_LLM_GATEWAY_VK=sk-bf-abc\n",
        "export LOOM_LLM_GATEWAY_VK='sk-bf-abc'\n",
        "LOOM_LLM_GATEWAY_VK=\"sk-bf-abc\"",
    ] {
        assert_eq!(parse_vk(text).unwrap(), "sk-bf-abc", "{text:?}");
    }
    // A key that itself contains `=` is not mistaken for an assignment.
    assert_eq!(parse_vk("YWJjZA==\n").unwrap(), "YWJjZA==");
    assert!(parse_vk("").is_err());
    assert!(parse_vk("a\nb\n").is_err());
    assert!(validate_vk("has space").is_err());
    assert!(validate_vk("").is_err());
}

#[cfg(unix)]
#[test]
fn the_key_file_must_be_absolute_owner_only_and_is_never_echoed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.vk");
    std::fs::write(&path, format!("{VK}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let plan = |path: &Path| Plan {
        url: URL.into(),
        profile: "quick-cerebras".into(),
        source: "CEREBRAS_API_KEY".into(),
        target: "CEREBRAS_API_KEY".into(),
        header: Some(DEFAULT_VK_HEADER.into()),
        vk: VkSource::File(path.to_path_buf()),
    };
    let error = plan(&path).open_with(&env_of(&[])).unwrap_err();
    assert!(error.message.contains("chmod 600"), "{}", error.message);
    assert!(!error.message.contains(VK), "{}", error.message);
    assert!(plan(&path).check_source().is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(plan(&path).check_source().is_ok());
    let route = plan(&path).open_with(&env_of(&[])).unwrap();
    assert_eq!(route.vk, VK);
    let rendered = format!("{route:?} {}", route.marker("pi"));
    assert!(!rendered.contains(VK), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    let error = plan(Path::new("relative/gateway.vk"))
        .open_with(&env_of(&[]))
        .unwrap_err();
    assert!(error.message.contains("absolute"), "{}", error.message);
}

// ---- what each harness receives ---------------------------------------------

fn route_for(runtime: &str, selection: &Selection) -> Route {
    let env = routed_env();
    plan_of(runtime, selection, &env)
        .unwrap()
        .unwrap()
        .open_with(&env)
        .unwrap()
}

#[test]
fn opencode_gets_base_url_key_reference_and_header_in_its_provider_options() {
    let mut selection = zai_opencode();
    selection.provider_options = Some(
        json!({"region": "kept", "headers": {"x-kept": "1"}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let route = route_for("opencode", &selection);
    route.adapt("opencode", &mut selection);
    let options = Value::Object(selection.provider_options.clone().unwrap());
    assert_eq!(options["baseURL"], URL);
    assert_eq!(options["apiKey"], "{env:ZHIPU_API_KEY}");
    assert_eq!(options["headers"]["x-bf-vk"], "{env:ZHIPU_API_KEY}");
    assert_eq!(options["headers"]["x-kept"], "1");
    assert_eq!(options["region"], "kept");
    assert!(!options.to_string().contains(VK));
    // The key rides under the profile's own target; the source is dropped.
    let mut cmd = Command::new("/bin/true");
    route.credential().apply(&mut cmd);
    route
        .finish("opencode", &selection.provider, &mut cmd)
        .unwrap();
    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
        })
        .collect();
    assert!(envs.contains(&("ZHIPU_API_KEY".into(), Some(VK.into()))), "{envs:?}");
    assert!(envs.contains(&("ZAI_API_KEY".into(), None)), "{envs:?}");
    assert_eq!(route.credential().source.as_str(), "gateway");
}

#[test]
fn kimi_gets_the_base_url_and_pi_gets_no_provider_options() {
    let mut kimi = api_key_selection("quick-cerebras");
    let route = route_for("kimi", &kimi);
    route.adapt("kimi", &mut kimi);
    assert_eq!(Value::Object(kimi.provider_options.unwrap())["baseUrl"], URL);
    // The Pi harness refuses providerOptions; Pi's override is models.json.
    let mut pi = api_key_selection("quick-cerebras");
    let route = route_for("pi", &pi);
    route.adapt("pi", &mut pi);
    assert!(pi.provider_options.is_none());
}

#[test]
fn pi_gets_a_private_models_json_override_or_refuses_unguarded() {
    let selection = api_key_selection("quick-cerebras");
    let route = route_for("pi", &selection);
    // Unguarded: no Loom-owned agent directory on the command.
    let mut unguarded = Command::new("/bin/true");
    let error = route.finish("pi", "cerebras", &mut unguarded).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error.message.contains("unguarded"), "{}", error.message);
    let dir = tempfile::tempdir().unwrap();
    let mut guarded = Command::new("/bin/true");
    guarded.env("PI_CODING_AGENT_DIR", dir.path());
    route.finish("pi", "cerebras", &mut guarded).unwrap();
    let text = std::fs::read_to_string(dir.path().join("models.json")).unwrap();
    let models: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(models["providers"]["cerebras"]["baseUrl"], URL);
    assert_eq!(models["providers"]["cerebras"]["apiKey"], "${CEREBRAS_API_KEY}");
    assert_eq!(models["providers"]["cerebras"]["headers"]["x-bf-vk"], "${CEREBRAS_API_KEY}");
    assert!(!text.contains(VK), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("models.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}
