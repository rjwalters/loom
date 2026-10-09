//! Route opted-in API-key model profiles through a self-hosted LLM gateway
//! (issue #9473).
//!
//! A host can hand the native harnesses Loom spawns a gateway base URL and a
//! **virtual key** in place of each provider's own endpoint and key, so the
//! metered API-key traffic is governed, budgeted and traced in one place
//! instead of per-host harness config. The deployment this was built for is
//! 2AMLogic/2am decision D39 (a self-hosted Bifrost, one virtual key per
//! consumer class). Nothing here is Bifrost-specific beyond the default
//! virtual-key header name. Operator reference: `defaults/docs/llm-gateway.md`.
//!
//! # Contract (env > config > off)
//!
//! | Setting | Environment | `runtimes.llmGateway.*` | Neither |
//! |---|---|---|---|
//! | Gateway base URL | `LOOM_LLM_GATEWAY_URL` (`off` disables) | `url` | feature off |
//! | Routed profiles | `LOOM_LLM_GATEWAY_PROFILES` | `profiles` | nothing routed |
//! | Virtual key | `LOOM_LLM_GATEWAY_VK` (value), else `LOOM_LLM_GATEWAY_VK_FILE` (path) | `virtualKeyFile` | a routed launch refuses (78) |
//! | Virtual-key header | `LOOM_LLM_GATEWAY_VK_HEADER` (`none` = bearer only) | `virtualKeyHeader` | `x-bf-vk` |
//!
//! The virtual key itself is never read from configuration, which is
//! committed. Routing is opt-in **per model profile**: a host turns it on for
//! the named profiles alone, so a canary host is one environment variable.
//!
//! # Who may receive it (the red line)
//!
//! Only a native harness ([`MAPPED_RUNTIMES`]) launching a model profile that
//! the host opted in by name **and** that authenticates with exactly one
//! API-key variable. Claude Code and Codex never do: they run on subscription
//! seats (the Claude OAuth token pool, Codex's ChatGPT login), and pushing a
//! subscription credential through a gateway is the shape Anthropic's terms
//! forbid (2am D39). That is enforced three ways, each tested:
//!
//! 1. [`plan`] answers `None` for every [`NEVER_RUNTIMES`] entry before it
//!    reads a single setting, refuses a profile with no single API key, and
//!    refuses an Anthropic credential presented as the virtual key.
//! 2. `spawn-worker` ([`super::run`]) removes every [`ENV_NAMES`] variable
//!    from the command it execs, native harness and legacy adapter alike, so
//!    `spawn-claude.sh` / `spawn-codex.sh` never see them, and a routed native
//!    harness holds the key only under its own provider-key variable.
//! 3. The daemon's dispatch surfaces (sweep spawn, role tick, epic role
//!    dispatch) call [`guard_dispatch`], which strips the same variables from
//!    a child admitted for a runtime that cannot use them, or spawned through
//!    anything other than `spawn-worker.sh`.
//!
//! # What each harness receives
//!
//! The key always replaces the profile's own provider-key variable (the
//! profile's `credentialTargets` entry, e.g. `CEREBRAS_API_KEY`), so the
//! harness presents it as `Authorization: Bearer <vk>`; the API-key pool is
//! not consulted and the real provider key never reaches the child.
//!
//! - **OpenCode**: `provider.<id>.options.baseURL` = the gateway URL,
//!   `options.apiKey` = `{env:<target>}`, and `options.headers.<header>` =
//!   `{env:<target>}` in the per-launch `OPENCODE_CONFIG_CONTENT`.
//! - **Pi**: a per-launch `models.json` in Loom's private `PI_CODING_AGENT_DIR`
//!   overriding the built-in provider's `baseUrl`, `apiKey` (`${<target>}`) and
//!   `headers`. Needs a guarded (role-tagged) launch, which every daemon
//!   dispatch is; an unguarded Pi launch of a routed profile refuses.
//! - **Kimi**: `KIMI_MODEL_BASE_URL` = the gateway URL (via the
//!   `providerOptions.kimi.baseUrl` translation). Kimi has no custom-header
//!   surface, so the key travels as the bearer key only.
//!
//! The model name is sent unchanged: the gateway's virtual key must route the
//! profile's bare model to the right provider (Bifrost: an explicit
//! `allowed_models` entry on the virtual key's provider config).

use super::{credential::Resolved, profiles::Selection, LaunchError};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Gateway base URL handed to a routed harness. `off` disables routing.
pub const URL_ENV: &str = "LOOM_LLM_GATEWAY_URL";
/// Comma/space-separated model profile names routed through the gateway.
pub const PROFILES_ENV: &str = "LOOM_LLM_GATEWAY_PROFILES";
/// The virtual key itself. Prefer [`VK_FILE_ENV`] for a long-lived daemon.
pub const VK_ENV: &str = "LOOM_LLM_GATEWAY_VK";
/// Absolute path to an owner-only file holding the virtual key.
pub const VK_FILE_ENV: &str = "LOOM_LLM_GATEWAY_VK_FILE";
/// Extra request header carrying the virtual key (`none` for bearer only).
pub const VK_HEADER_ENV: &str = "LOOM_LLM_GATEWAY_VK_HEADER";
/// Every variable of the contract: scrubbed from every spawned harness.
pub const ENV_NAMES: [&str; 5] = [URL_ENV, PROFILES_ENV, VK_ENV, VK_FILE_ENV, VK_HEADER_ENV];
/// Bifrost's virtual-key header, the default extra header.
pub const DEFAULT_VK_HEADER: &str = "x-bf-vk";
/// Runtimes that must never be routed, named so the red line is explicit and
/// survives a future native adapter for either of them.
pub const NEVER_RUNTIMES: [&str; 2] = ["claude", "codex"];
/// Native harnesses with a gateway mapping. A closed list, not a denylist.
pub const MAPPED_RUNTIMES: [&str; 3] = ["pi", "opencode", "kimi"];
/// The one launcher that maps (or scrubs) the contract itself.
const SEAM: &str = "spawn-worker.sh";
/// Headers that already mean something else on a provider request.
const RESERVED_HEADERS: [&str; 5] = [
    "authorization",
    "host",
    "content-type",
    "content-length",
    "cookie",
];
const MAX_VK_BYTES: usize = 4096;
const MAX_VK_FILE_BYTES: u64 = 64 * 1024;

#[cfg(test)]
mod tests;

/// Whether `runtime` can ever receive a gateway mapping.
#[must_use]
pub fn runtime_eligible(runtime: &str) -> bool {
    !NEVER_RUNTIMES.contains(&runtime) && MAPPED_RUNTIMES.contains(&runtime)
}

/// Remove every gateway variable from `cmd`'s environment.
pub fn scrub(cmd: &mut Command) {
    for name in ENV_NAMES {
        cmd.env_remove(name);
    }
}

/// Dispatch-surface guard: keep the contract out of a child that cannot use
/// it. The child keeps it only when it is `spawn-worker.sh` (which maps or
/// scrubs it per launch) **and** its admitted runtime, when known, is a mapped
/// native harness. An unknown runtime is left to `spawn-worker`, which scrubs
/// it from every legacy adapter itself.
pub fn guard_dispatch(cmd: &mut Command, spawn_bin: &Path, runtime: Option<&str>) {
    let through_seam = spawn_bin.file_name().is_some_and(|name| name == SEAM);
    if through_seam && runtime.is_none_or(runtime_eligible) {
        return;
    }
    scrub(cmd);
}

/// `runtimes.llmGateway`. Closed schema: a key outside it (above all a
/// virtual key pasted into committed configuration) is a configuration error.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigBlock {
    url: Option<String>,
    #[serde(default)]
    profiles: Vec<String>,
    virtual_key_file: Option<PathBuf>,
    virtual_key_header: Option<String>,
}

/// Where a routed launch's virtual key comes from. Names only: the value is
/// read by [`Plan::open`], once, immediately before launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VkSource {
    /// [`VK_ENV`].
    Env,
    /// [`VK_FILE_ENV`], else `runtimes.llmGateway.virtualKeyFile`.
    File(PathBuf),
}

impl VkSource {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::File(_) => "file",
        }
    }
}

struct Settings {
    url: String,
    profiles: Vec<String>,
    vk: Option<VkSource>,
    header: Option<String>,
}

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn process_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn settings(env: Env, config: &Value) -> Result<Option<Settings>, LaunchError> {
    let block = match config.pointer("/runtimes/llmGateway") {
        None | Some(Value::Null) => ConfigBlock::default(),
        Some(value) => ConfigBlock::deserialize(value).map_err(|_| {
            LaunchError::config(
                "invalid runtimes.llmGateway configuration: expected an object with only url, \
                 profiles, virtualKeyFile and virtualKeyHeader (the virtual key itself never \
                 belongs in configuration)",
            )
        })?,
    };
    let url = env(URL_ENV)
        .or_else(|| block.url.map(|url| url.trim().to_string()))
        .filter(|url| !url.is_empty());
    let Some(url) = url else {
        return Ok(None);
    };
    if url.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let profiles = match env(PROFILES_ENV) {
        Some(raw) => raw
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect(),
        None => block
            .profiles
            .iter()
            .map(|p| p.trim().to_string())
            .collect(),
    };
    let vk = if env(VK_ENV).is_some() {
        Some(VkSource::Env)
    } else {
        env(VK_FILE_ENV)
            .map(PathBuf::from)
            .or(block.virtual_key_file)
            .map(VkSource::File)
    };
    let header = env(VK_HEADER_ENV).or(block.virtual_key_header);
    Ok(Some(Settings {
        url,
        profiles,
        vk,
        header,
    }))
}

/// A routed launch, decided without reading the virtual key.
#[derive(Debug)]
pub struct Plan {
    /// Validated base URL, without a trailing `/`.
    pub url: String,
    pub profile: String,
    /// The profile's credential source variable (never set on the child).
    pub source: String,
    /// The child variable that carries the virtual key.
    pub target: String,
    /// Extra header carrying the key; `None` sends it as the bearer key only.
    pub header: Option<String>,
    pub vk: VkSource,
}

/// Decide whether this launch is routed. `Ok(None)` launches exactly as
/// before; `Err` (78) is a profile the host opted in that cannot be routed
/// safely, refused rather than launched around the gateway.
pub fn plan(
    runtime: &str,
    selection: &Selection,
    config: &Value,
) -> Result<Option<Plan>, LaunchError> {
    plan_with(runtime, selection, config, &process_env)
}

fn plan_with(
    runtime: &str,
    selection: &Selection,
    config: &Value,
    env: Env,
) -> Result<Option<Plan>, LaunchError> {
    // The red line comes first, before any setting is read.
    if NEVER_RUNTIMES.contains(&runtime) {
        return Ok(None);
    }
    let Some(settings) = settings(env, config)? else {
        return Ok(None);
    };
    let Some(profile) = selection.profile.as_deref() else {
        return Ok(None);
    };
    if !settings.profiles.iter().any(|name| name == profile) {
        return Ok(None);
    }
    let refuse = |why: &str| {
        LaunchError::config(format!(
            "model profile '{profile}' is routed through the LLM gateway ({PROFILES_ENV} or \
             runtimes.llmGateway.profiles), but {why}. Refusing rather than launching around \
             the gateway; see .loom/docs/llm-gateway.md"
        ))
    };
    if !MAPPED_RUNTIMES.contains(&runtime) {
        return Err(refuse(&format!("the {runtime} harness has no gateway mapping")));
    }
    let ([source], [(_, target)]) =
        (selection.credential_sources.as_slice(), selection.credentials.as_slice())
    else {
        return Err(refuse(
            "it does not authenticate with exactly one API-key variable (credentialEnv), so it \
             is not a metered or API-key provider; a subscription or harness-login profile \
             never traverses the gateway",
        ));
    };
    if [source, target]
        .iter()
        .any(|name| name.to_ascii_uppercase().contains("OAUTH"))
    {
        return Err(refuse(
            "its credential variable names an OAuth/subscription token, which never traverses \
             the gateway",
        ));
    }
    if selection.credential_proxy.is_some() {
        return Err(refuse(
            "it also declares credentialProxy, which re-points the same provider endpoint",
        ));
    }
    let url = validate_url(&settings.url)
        .map_err(|why| refuse(&format!("the gateway URL is unusable ({why})")))?;
    let header = validate_header(settings.header.as_deref()).map_err(|why| refuse(why))?;
    let vk = settings.vk.ok_or_else(|| {
        refuse(&format!(
            "no virtual key is configured: set {VK_FILE_ENV} (an owner-only file) or {VK_ENV}"
        ))
    })?;
    Ok(Some(Plan {
        url,
        profile: profile.to_string(),
        source: source.clone(),
        target: target.clone(),
        header,
        vk,
    }))
}

fn validate_url(raw: &str) -> Result<String, &'static str> {
    super::egress_proxy::Upstream::parse(raw)?;
    Ok(raw.trim().trim_end_matches('/').to_string())
}

fn validate_header(raw: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(name) = raw.map(str::trim) else {
        return Ok(Some(DEFAULT_VK_HEADER.to_string()));
    };
    if name.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(
            "the virtual-key header must be an HTTP header name (letters, digits, '-') or none",
        );
    }
    if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err("the virtual-key header names a reserved request header; the key already travels as the bearer key");
    }
    Ok(Some(name.to_string()))
}

impl Plan {
    /// Secret-free one-line summary for `worker profile-check` and the
    /// per-launch marker.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "url={} credential={} vk={} header={}",
            self.url,
            self.target,
            self.vk.as_str(),
            self.header.as_deref().unwrap_or("none")
        )
    }

    /// Check the key's source without reading the key (`worker profile-check`
    /// reads no secrets).
    pub fn check_source(&self) -> Result<(), String> {
        match &self.vk {
            VkSource::Env => Ok(()),
            VkSource::File(path) => check_vk_file(path).map(|_| ()),
        }
    }

    /// Read and validate the virtual key. The value is never echoed.
    pub fn open(self) -> Result<Route, LaunchError> {
        self.open_with(&process_env)
    }

    fn open_with(self, env: Env) -> Result<Route, LaunchError> {
        let refuse = |why: &str| {
            LaunchError::config(format!(
                "LLM gateway virtual key for model profile '{}': {why}",
                self.profile
            ))
        };
        let value = match &self.vk {
            VkSource::Env => env(VK_ENV).ok_or_else(|| refuse(&format!("{VK_ENV} is empty")))?,
            VkSource::File(path) => read_vk_file(path).map_err(|why| refuse(&why))?,
        };
        validate_vk(&value).map_err(|why| refuse(why))?;
        Ok(Route {
            plan: self,
            vk: value,
        })
    }
}

fn check_vk_file(path: &Path) -> Result<u64, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{VK_FILE_ENV} / runtimes.llmGateway.virtualKeyFile must be an absolute path"
        ));
    }
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("cannot read {}: {:?}", path.display(), e.kind()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{} is readable by group or others; make it owner-only (chmod 600)",
                path.display()
            ));
        }
    }
    if meta.len() > MAX_VK_FILE_BYTES {
        return Err(format!("{} is too large to hold one key", path.display()));
    }
    Ok(meta.len())
}

fn read_vk_file(path: &Path) -> Result<String, String> {
    check_vk_file(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {:?}", path.display(), e.kind()))?;
    parse_vk(&text).map_err(str::to_string)
}

/// A bare value, or one `LOOM_LLM_GATEWAY_VK=value` line (so the same file can
/// serve as an `EnvironmentFile`). Blank and `#` lines are ignored.
fn parse_vk(text: &str) -> Result<String, &'static str> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return Err("the virtual-key file must hold exactly one key: a bare value or one LOOM_LLM_GATEWAY_VK=value line");
    };
    let line = line.strip_prefix("export ").map_or(line, str::trim_start);
    let value = line
        .strip_prefix(VK_ENV)
        .and_then(|rest| rest.strip_prefix('='))
        .unwrap_or(line)
        .trim();
    let unquoted = ['"', '\'']
        .iter()
        .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
        .unwrap_or(value);
    Ok(unquoted.to_string())
}

fn validate_vk(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("the key is empty");
    }
    if value.len() > MAX_VK_BYTES {
        return Err("the key is implausibly long");
    }
    if !value.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err("the key must be printable ASCII without whitespace");
    }
    // A virtual key is issued by the gateway. An Anthropic credential here is
    // either a Claude subscription token or a provider key in the wrong slot;
    // neither may be presented to the gateway as the caller's identity.
    if value.starts_with("sk-ant-") {
        return Err("the key is an Anthropic credential, not a gateway virtual key; a Claude credential never traverses the gateway");
    }
    Ok(())
}

/// A routed launch with its key in hand. `Debug` redacts the key.
pub struct Route {
    plan: Plan,
    vk: String,
}

impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Route")
            .field("plan", &self.plan)
            .field("vk", &"<redacted>")
            .finish()
    }
}

fn object_at<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let entry = map.entry(key).or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    entry.as_object_mut().expect("object entry")
}

impl Route {
    /// The credential the launch injects: the key under the profile's own
    /// provider-key variable, in place of the API-key ladder.
    #[must_use]
    pub fn credential(&self) -> Resolved {
        Resolved::gateway(self.plan.target.clone(), OsString::from(&self.vk))
    }

    /// Fold the gateway into the harness's per-launch provider options
    /// (OpenCode, Kimi) before the command is built. Pi has no provider-option
    /// surface; [`Self::finish`] writes its override instead.
    pub fn adapt(&self, runtime: &str, selection: &mut Selection) {
        let reference = format!("{{env:{}}}", self.plan.target);
        let options = selection.provider_options.get_or_insert_with(Map::new);
        match runtime {
            "opencode" => {
                options.insert("baseURL".into(), json!(self.plan.url));
                options.insert("apiKey".into(), json!(reference));
                if let Some(header) = &self.plan.header {
                    object_at(options, "headers").insert(header.clone(), json!(reference));
                }
            }
            "kimi" => {
                options.insert("baseUrl".into(), json!(self.plan.url));
            }
            _ => {
                if options.is_empty() {
                    selection.provider_options = None;
                }
            }
        }
    }

    /// Finish the harness command: drop the provider's own source variable
    /// (the gateway holds the real key) and, for Pi, write the per-launch
    /// `models.json` override into Loom's private agent directory.
    pub fn finish(
        &self,
        runtime: &str,
        provider: &str,
        command: &mut Command,
    ) -> Result<(), LaunchError> {
        if self.plan.source != self.plan.target {
            command.env_remove(&self.plan.source);
        }
        if runtime != "pi" {
            return Ok(());
        }
        let refuse = |why: &str| {
            LaunchError::config(format!(
                "model profile '{}' is routed through the LLM gateway, but {why}",
                self.plan.profile
            ))
        };
        if std::env::var_os("LOOM_NATIVE_AUTH_FILE").is_some_and(|v| !v.is_empty()) {
            return Err(refuse(
                "LOOM_NATIVE_AUTH_FILE is set: a stored Pi login would outrank the virtual key",
            ));
        }
        let dir = command
            .get_envs()
            .find(|(key, _)| *key == "PI_CODING_AGENT_DIR")
            .and_then(|(_, value)| value)
            .map(PathBuf::from)
            .ok_or_else(|| {
                refuse(
                    "this pi launch is unguarded: the override needs Loom's per-launch agent \
                     directory, which only a role-tagged launch provisions",
                )
            })?;
        let reference = format!("${{{}}}", self.plan.target);
        let mut entry = json!({"baseUrl": self.plan.url, "apiKey": reference});
        if let Some(header) = &self.plan.header {
            entry["headers"] = json!({ header.clone(): reference });
        }
        let models = json!({"providers": { provider: entry }});
        write_private(&dir.join("models.json"), &models.to_string())
            .map_err(|e| refuse(&format!("the Pi models.json override could not be written: {e}")))
    }

    /// Secret-free per-launch log marker, a sibling of `# LOOM_LAUNCH`.
    #[must_use]
    pub fn marker(&self, runtime: &str) -> String {
        format!(
            "# LOOM_LLM_GATEWAY runtime={runtime} profile={} {}",
            self.plan.profile,
            self.plan.summary()
        )
    }
}

fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(content.as_bytes())?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
