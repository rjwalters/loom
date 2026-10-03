//! The sealed registration (issue #10102): when Loom may pass Codex's
//! `--dangerously-bypass-hook-trust`, because it has vetted every hook source
//! the flag could let run.
//!
//! # Why the flag is safe only under these conditions
//!
//! Codex 0.160 describes the flag as "Run enabled hooks without requiring
//! persisted hook trust for this invocation… Intended only for automation that
//! already vets hook sources". Its discovery
//! (`codex-rs/hooks/src/engine/discovery.rs`, tag `rust-v0.160.0`) applies the
//! flag to every **non-managed** hook source, and only to those (managed
//! hooks never needed trust):
//!
//! | Source | Where Codex reads it | How this module closes it |
//! |---|---|---|
//! | user layer | `$CODEX_HOME/hooks.json`, `[hooks]` in `$CODEX_HOME/config.toml` | [`vet_hooks_json`] / [`vet_user_config`]: exactly Loom's one entry, no `[hooks]` events |
//! | project layers | `.codex/hooks.json` and `.codex/config.toml` from the cwd up to the project root, plus the main checkout's `.codex/` for a linked worktree | [`vet_project_layers`]: none present |
//! | session flags | `-c hooks…`, `--enable`/`--disable`, `--profile` | [`vet_session_flags`]: none present |
//! | plugins | enabled plugins' hook files, some installed remotely by the account's backend | not vetted. Removed instead: a bypassed launch also passes [`PLUGINS_OFF`] |
//!
//! #4495 forbade the flag because waiving trust would let a hook nobody
//! reviewed run outside the sandbox. The trust prompt exists to review hook
//! sources, and this module does that review mechanically, against the exact
//! bytes Codex will read. Three things #4495 did not have make that review
//! durable for the whole session:
//!
//! * **The container is the boundary** (operator ruling, #10014; #9979): the
//!   flag is only considered for `CODEX_HOME=`[`SESSION_CODEX_HOME`], inside a
//!   hardened session container. A bare-metal launch always needs recorded
//!   trust.
//! * **The profile controls are sealed**: `hooks.json`, `config.toml` and the
//!   receipt are read-only binds in that container (#9979), so nothing the
//!   session runs can add a hook for itself or for the next session.
//! * **Byte identity is proven**: `session-exec posture` (mode `host`) checked
//!   the binds and the host/container identity, and [`container_sees`] then
//!   checks that the container's copies are the bytes vetted here.
//!
//! Every check fails closed. Anything Loom didn't write, can't parse, or
//! can't see means no flag. The launch then needs recorded trust as before,
//! and without it a guarded role exits 78 and the daemon routes to the next
//! runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::{loom_trust_keys, sha256_hex, RECEIPT, SESSION_CODEX_HOME, SHARED_COMMAND};
use crate::tokens_pool::private_workspace::bundle::PROFILE_CONTROLS;

/// Codex's trust waiver, passed only for a sealed registration.
pub const BYPASS_FLAG: &str = "--dangerously-bypass-hook-trust";
/// Passed with [`BYPASS_FLAG`]: plugin hook sources can't be vetted (some come
/// from the account's backend), so a bypassed launch runs without plugins.
pub const PLUGINS_OFF: [&str; 2] = ["-c", "features.plugins=false"];
/// The managed entry's timeout bounds. Loom writes 30 by default. A shorter
/// one could time the guard out, and a timed-out hook is a hook failure,
/// not a block.
pub const MIN_TIMEOUT: i64 = 30;
/// See [`MIN_TIMEOUT`].
pub const MAX_TIMEOUT: i64 = 600;
/// Config keys that add, remove or redirect hook sources (or the project root
/// that bounds project-layer discovery). Refused in `-c` overrides, in project
/// `.codex/config.toml`, and inside `profiles.*` of the user config.
const HOOK_SOURCE_KEYS: [&str; 7] = [
    "hooks",
    "features",
    "plugins",
    "marketplaces",
    "project_root_markers",
    "profile",
    "profiles",
];

/// What a sealed check is asked about one launch.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The directory Codex starts in (its cwd). Project layers are found from
    /// here. `None` means nothing to vet them against, so the seal is refused.
    pub launch_dir: Option<PathBuf>,
    /// The arguments the launch hands Codex (the session-flags layer).
    pub codex_args: Vec<String>,
    /// The session container to prove byte identity in. `None` vets the files
    /// only (the daemon's routing gate); `spawn-codex.sh` passes the flag
    /// only after a container proof.
    pub container: Option<Container>,
}

/// A session container and the docker binary that reaches it.
#[derive(Debug, Clone)]
pub struct Container {
    pub docker: String,
    pub name: String,
}

/// A vetted registration: the SHA-256 of each profile control's vetted bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seal {
    pub controls: BTreeMap<String, String>,
}

/// Vet `codex_home` as Codex will read it with `CODEX_HOME=runtime_home` for
/// this launch. `Err` carries a pathless, secret-free reason.
///
/// # Errors
/// Whenever any hook source other than Loom's sealed entry could run, or a
/// source cannot be read or parsed.
pub fn vet(codex_home: &Path, runtime_home: &Path, request: &Request) -> Result<Seal, String> {
    if runtime_home != Path::new(SESSION_CODEX_HOME) {
        return Err("the trust waiver is only used inside a hardened session container; this \
                    launch runs on the host, where recorded hook trust is required"
            .into());
    }
    let launch = request
        .launch_dir
        .as_deref()
        .ok_or("no launch directory was named, so project-layer hook sources cannot be vetted")?;
    let mut bytes = BTreeMap::new();
    for name in PROFILE_CONTROLS {
        let read = std::fs::read(codex_home.join(name))
            .map_err(|_| format!("the profile's {name} is missing or unreadable"))?;
        bytes.insert(name, read);
    }
    let hooks = vet_hooks_json(&bytes["hooks.json"])?;
    vet_receipt(&bytes[RECEIPT])?;
    vet_user_config(&bytes["config.toml"], &hooks, runtime_home)?;
    vet_session_flags(&request.codex_args)?;
    vet_project_layers(launch)?;
    Ok(Seal {
        controls: bytes
            .into_iter()
            .map(|(name, read)| (name.to_owned(), sha256_hex(&read)))
            .collect(),
    })
}

/// Loom's `hooks.json` shape, and nothing else: one `PreToolUse` group
/// matching every tool, holding one command handler. `deny_unknown_fields` and
/// required fields refuse any other event, key, handler option (`async`,
/// `statusMessage`) or handler type. Serde also refuses a duplicated key,
/// which a lenient JSON reader would silently resolve to its last value,
/// while Codex's own loader would refuse the whole file and so run no hooks.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksFile {
    hooks: Events,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Events {
    #[serde(rename = "PreToolUse")]
    pre_tool_use: Vec<Group>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Group {
    matcher: String,
    hooks: Vec<Handler>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Handler {
    #[serde(rename = "type")]
    kind: String,
    command: String,
    timeout: i64,
}

/// `hooks.json` is exactly Loom's sealed registration. Returns the parsed
/// value (for the trust keys Loom's entry occupies).
///
/// # Errors
/// On any byte that is not Loom's entry.
pub fn vet_hooks_json(bytes: &[u8]) -> Result<serde_json::Value, String> {
    let file: HooksFile = serde_json::from_slice(bytes).map_err(|_| {
        let installed = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|hooks| super::loom_command(&hooks))
            .is_some();
        if installed {
            "hooks.json registers something other than Loom's one managed PreToolUse entry (an \
             extra event, key, handler option, or a duplicated key)"
        } else {
            "Loom's managed PreToolUse hook is not installed in this profile"
        }
        .to_owned()
    })?;
    let [group] = file.hooks.pre_tool_use.as_slice() else {
        return Err("hooks.json carries more than Loom's one PreToolUse group".into());
    };
    if group.matcher != "*" {
        return Err(
            "the managed entry's matcher is not \"*\", so some tools would skip Loom's guard"
                .into(),
        );
    }
    let [handler] = group.hooks.as_slice() else {
        return Err("Loom's PreToolUse group carries more than Loom's one handler".into());
    };
    if handler.kind != "command" || handler.command != SHARED_COMMAND {
        return Err("the managed handler is not Loom's workspace-independent command".into());
    }
    if !(MIN_TIMEOUT..=MAX_TIMEOUT).contains(&handler.timeout) {
        return Err(format!(
            "the managed handler's timeout is outside {MIN_TIMEOUT}..={MAX_TIMEOUT} seconds"
        ));
    }
    serde_json::from_slice(bytes).map_err(|_| "hooks.json is not JSON".into())
}

/// The receipt pins Loom's command.
fn vet_receipt(bytes: &[u8]) -> Result<(), String> {
    let receipt: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| "the managed-hook receipt is not JSON")?;
    if receipt["loomManagedHook"]["commandSha256"].as_str()
        != Some(sha256_hex(SHARED_COMMAND.as_bytes()).as_str())
    {
        return Err("the managed-hook receipt does not pin Loom's command".into());
    }
    Ok(())
}

/// The user `config.toml` adds no hook source and doesn't switch Loom's off.
///
/// # Errors
/// When it parses as anything Codex might read differently, or touches hooks.
pub fn vet_user_config(
    bytes: &[u8],
    hooks: &serde_json::Value,
    runtime_home: &Path,
) -> Result<(), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "config.toml is not UTF-8")?;
    let config: toml::Table = text.parse().map_err(|_| "config.toml does not parse")?;
    if let Some(section) = config.get("hooks") {
        let table = section
            .as_table()
            .ok_or("config.toml's `hooks` is not a table")?;
        if table.keys().any(|key| key != "state") {
            return Err("config.toml defines hooks of its own (a `[hooks]` key other than \
                        `state`)"
                .into());
        }
        let state = table.get("state").map_or_else(
            || Ok(toml::Table::new()),
            |state| {
                state
                    .as_table()
                    .cloned()
                    .ok_or("config.toml's `hooks.state` is not a table")
            },
        )?;
        for key in loom_trust_keys(hooks, runtime_home) {
            if state.get(&key).and_then(|entry| entry.get("enabled"))
                == Some(&toml::Value::Boolean(false))
            {
                return Err("config.toml disables Loom's managed hook (enabled = false)".into());
            }
        }
    }
    if let Some(features) = config.get("features") {
        let features = features
            .as_table()
            .ok_or("config.toml's `features` is not a table")?;
        for key in ["hooks", "codex_hooks"] {
            if features
                .get(key)
                .is_some_and(|value| value.as_bool() != Some(true))
            {
                return Err(format!(
                    "config.toml sets features.{key} to something other than true"
                ));
            }
        }
    }
    if config.contains_key("project_root_markers") {
        return Err("config.toml moves the project root (project_root_markers), which bounds \
                    project-layer hook discovery"
            .into());
    }
    if let Some(profiles) = config.get("profiles") {
        let profiles = profiles
            .as_table()
            .ok_or("config.toml's `profiles` is not a table")?;
        let touches = |profile: &toml::Value| {
            profile
                .as_table()
                .is_none_or(|table| HOOK_SOURCE_KEYS.iter().any(|key| table.contains_key(*key)))
        };
        if profiles.values().any(touches) {
            return Err("a config.toml profile touches hooks, features or plugins".into());
        }
    }
    Ok(())
}

/// No argument handed to Codex adds, removes or redirects a hook source, or
/// asks for the waiver on its own.
///
/// # Errors
/// On the first such argument.
pub fn vet_session_flags(args: &[String]) -> Result<(), String> {
    let mut iter = args.iter().map(String::as_str);
    while let Some(arg) = iter.next() {
        let value = match arg {
            BYPASS_FLAG => {
                return Err(format!(
                    "the launch already carries {BYPASS_FLAG}; only Loom's own vetting may add it"
                ))
            }
            "-p" | "--profile" | "--enable" | "--disable" => {
                return Err(format!("the launch passes {arg}, which can change hook sources"));
            }
            _ if ["-p=", "--profile=", "--enable=", "--disable="]
                .iter()
                .any(|prefix| arg.starts_with(prefix)) =>
            {
                return Err("the launch passes a profile or feature switch, which can change \
                            hook sources"
                    .into());
            }
            "-c" | "--config" => iter.next().unwrap_or_default(),
            _ if arg.starts_with("--config=") => &arg["--config=".len()..],
            _ if arg.starts_with("-c") => arg[2..].trim_start_matches('='),
            _ => continue,
        };
        let key = value.split('=').next().unwrap_or_default();
        let head = key
            .split('.')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"');
        if HOOK_SOURCE_KEYS.contains(&head) || head == "bypass_hook_trust" {
            return Err(format!(
                "the launch overrides `{head}` with -c, which can change hook \
                                sources"
            ));
        }
    }
    Ok(())
}

/// No project layer Codex would load for a launch in `launch` carries a hook
/// source. Codex walks from the cwd up to the project root (the first
/// ancestor with a `.git`), and for a linked worktree reads each layer's
/// hooks from the main checkout's matching `.codex/`. Both sets are checked,
/// for the launch path as given and as resolved.
///
/// # Errors
/// When a `.codex/hooks.json` exists, a `.codex/config.toml` touches hooks or
/// cannot be read, or the main checkout of a worktree cannot be resolved.
pub fn vet_project_layers(launch: &Path) -> Result<(), String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let resolved = launch
        .canonicalize()
        .map_err(|_| "the launch directory cannot be resolved")?;
    for start in [launch.to_path_buf(), resolved] {
        let is_root = |dir: &Path| {
            let git = dir.join(".git");
            git.is_file() || git.join("HEAD").exists()
        };
        let Some(root) = start
            .ancestors()
            .find(|dir| is_root(dir))
            .map(Path::to_path_buf)
        else {
            // No project root: Codex reads only the cwd's own `.codex/`.
            dirs.push(start);
            continue;
        };
        for dir in start.ancestors() {
            dirs.push(dir.to_path_buf());
            if dir == root {
                break;
            }
        }
        if root.join(".git").is_file() {
            let main = main_checkout(&root)?;
            for dir in start.ancestors() {
                let Ok(rel) = dir.strip_prefix(&root) else {
                    break;
                };
                dirs.push(main.join(rel));
            }
        }
    }
    for dir in dirs {
        let dot = dir.join(".codex");
        if dot.join("hooks.json").symlink_metadata().is_ok() {
            return Err(
                "the checkout carries a project .codex/hooks.json, a hook source Loom did \
                        not write"
                    .into(),
            );
        }
        let config = dot.join("config.toml");
        if config.symlink_metadata().is_err() {
            continue;
        }
        let table: toml::Table = std::fs::read_to_string(&config)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or("the checkout's project .codex/config.toml cannot be read or parsed")?;
        if HOOK_SOURCE_KEYS.iter().any(|key| table.contains_key(*key)) {
            return Err("the checkout's project .codex/config.toml touches hooks, features or \
                        plugins"
                .into());
        }
    }
    Ok(())
}

/// The main checkout of the linked worktree rooted at `root`.
fn main_checkout(root: &Path) -> Result<PathBuf, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .map_err(|_| "git is unavailable, so a worktree's main checkout cannot be vetted")?;
    let common = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || common.is_empty() {
        return Err("a worktree's main checkout cannot be resolved, so its hooks cannot be \
                    vetted"
            .into());
    }
    Path::new(&common)
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "a worktree's main checkout cannot be resolved".into())
}

/// The session container reads exactly the bytes [`vet`] vetted: hash each
/// profile control inside it (`docker exec … sha256sum`) and compare.
///
/// # Errors
/// When docker cannot answer, or any control differs or is missing.
pub fn container_sees(container: &Container, seal: &Seal) -> Result<(), String> {
    let paths: Vec<String> = PROFILE_CONTROLS
        .iter()
        .map(|name| format!("{SESSION_CODEX_HOME}/{name}"))
        .collect();
    let output = Command::new(&container.docker)
        .args(["exec", &container.name, "sha256sum", "--"])
        .args(&paths)
        .output()
        .map_err(|_| "docker could not be asked for the session container's profile controls")?;
    let seen =
        crate::session_exec::posture::parse_sha256sum(&String::from_utf8_lossy(&output.stdout));
    for name in PROFILE_CONTROLS {
        let inside = seen.get(&format!("{SESSION_CODEX_HOME}/{name}"));
        if inside.is_none() || inside != seal.controls.get(name) {
            return Err(format!(
                "the session container's {name} is not the bytes Loom vetted on the host"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "codex_hooks_seal_tests.rs"]
mod tests;
