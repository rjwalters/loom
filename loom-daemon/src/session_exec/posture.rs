//! `loom-daemon session-exec posture` (issue #9979): may Codex run with its
//! own sandbox off inside this session container?
//!
//! Codex's `read-only`/`workspace-write` sandbox is bubblewrap, which cannot
//! create a user namespace in a session container (Docker's default seccomp
//! profile; Ubuntu's `apparmor_restrict_unprivileged_userns`). The operator
//! ruled (2026-10-03) that the hardened container is Codex's boundary, so
//! `spawn-codex.sh` runs `-s danger-full-access` there — but only after this
//! subcommand has checked, from `docker inspect`, that the container really is
//! hardened. The label says how the container was MEANT to be created; the
//! `HostConfig` says how it was. Both must agree (a label alone is not a
//! boundary), the same way `private_workspace::docker::validate_settings`
//! refuses to reuse a private-clone container whose settings drifted.
//!
//! The profile's hook-control files ([`PROFILE_CONTROLS`]) are part of that
//! posture: with Codex's sandbox off, a session that could write its own
//! `hooks.json`/`config.toml`/`loom-codex-hooks.json` could leave the NEXT
//! session reading guard-ready while Codex skips Loom's hook. So each must be
//! a read-only bind (from `docker inspect`), and in a host-mode container
//! the container's view of each must be byte-identical to the host file
//! `spawn-codex.sh` verified (`docker exec … sha256sum`): a file bind does not
//! follow a host-side atomic replace (provisioning, accepting hook trust), so
//! after one the container sees a stale or missing file while the host reads
//! ready.
//!
//! Output (stdout, one line): `mode=<m> sandbox=<s> gh=<forward|skip>`.
//! Refusals print their reason to stderr and exit 78 (EX_CONFIG).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::tokens_pool::profile_ledger::sha256_hex;
use crate::tokens_pool::session_lifecycle::{
    profile_control_destination, PROFILE_CONTROLS, SESSION_POSTURE, SESSION_POSTURE_LABEL,
};

/// Arguments for `session-exec posture`.
#[derive(clap::Args)]
pub struct PostureArgs {
    /// The session container to inspect.
    #[arg(long)]
    pub container: String,
    /// Account/profile name, for the recreate instructions.
    #[arg(long)]
    pub profile: String,
    /// The sandbox mode the dispatch resolved before the container was known.
    #[arg(long)]
    pub requested: String,
    /// The account profile on the host (`CODEX_HOME` as `spawn-codex.sh`
    /// verified it). Required for a host-mode container, whose view of the
    /// profile controls is compared with these files.
    #[arg(long)]
    pub codex_home: Option<PathBuf>,
    /// Docker binary (test seam; `LOOM_CODEX_SESSION_DOCKER`).
    #[arg(long, default_value = "docker")]
    pub docker: String,
}

/// What kind of container this is, as far as dropping the sandbox goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Posture {
    /// Missing, stopped or unreadable: nothing will be dispatched into it
    /// (`session-exec host` refuses), so the sandbox is left as requested.
    NotRunning,
    /// Running, but carrying neither posture label: created before #9979.
    Unhardened,
    /// Labelled hardened, but these `HostConfig` settings say otherwise.
    Mismatch(Vec<String>),
    /// A hardened private-clone container (#8787).
    PrivateClone,
    /// A hardened host-mode container (`container-boundary-v1`).
    Host,
}

impl Posture {
    fn mode(&self) -> &'static str {
        match self {
            Self::NotRunning => "not-running",
            Self::Unhardened => "unhardened",
            Self::Mismatch(_) => "posture-mismatch",
            Self::PrivateClone => "private-clone",
            Self::Host => "host",
        }
    }
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Every way `state`'s actual settings fall short of the container boundary.
/// Empty means hardened.
#[must_use]
pub fn violations(state: &Value) -> Vec<String> {
    let hc = &state["HostConfig"];
    let mut found = Vec::new();
    if hc["Privileged"] != Value::Bool(false) {
        found.push(format!("privileged={}", hc["Privileged"]));
    }
    for (key, name) in [
        ("NetworkMode", "network"),
        ("PidMode", "pid"),
        ("IpcMode", "ipc"),
        ("UTSMode", "uts"),
        ("UsernsMode", "userns"),
    ] {
        if hc[key] == "host" {
            found.push(format!("host-namespace({name})"));
        }
    }
    if !strings(&hc["CapDrop"])
        .iter()
        .any(|cap| cap.eq_ignore_ascii_case("ALL"))
    {
        found.push("cap-drop-ALL-missing".into());
    }
    let added = strings(&hc["CapAdd"]);
    if !added.is_empty() {
        found.push(format!("cap-add={}", added.join(",")));
    }
    let opts = strings(&hc["SecurityOpt"]);
    if !opts
        .iter()
        .any(|opt| *opt == "no-new-privileges" || *opt == "no-new-privileges:true")
    {
        found.push("no-new-privileges-missing".into());
    }
    for opt in opts.iter().filter(|opt| opt.contains("unconfined")) {
        found.push(format!("security-opt={opt}"));
    }
    if hc["Devices"].as_array().is_some_and(|d| !d.is_empty()) {
        found.push("devices".into());
    }
    let mounts = state["Mounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if mounts.iter().any(|m| {
        [&m["Source"], &m["Destination"]]
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.contains("docker.sock")))
    }) {
        found.push("docker-socket-mounted".into());
    }
    for name in PROFILE_CONTROLS {
        let destination = profile_control_destination(name);
        let frozen = mounts.iter().any(|m| {
            m["Destination"] == destination.as_str()
                && m["Type"] == "bind"
                && m["RW"] == Value::Bool(false)
        });
        if !frozen {
            found.push(format!("profile-control-writable({name})"));
        }
    }
    found
}

/// The container's view of the profile controls, or why it could not be
/// read. `Ok` holds the names whose container copy differs from (or is
/// missing beside) the host file.
pub type ControlDrift = Result<Vec<String>, String>;

/// `sha256sum` output (`<hex>  <path>` per line) as path → hex.
#[must_use]
pub fn parse_sha256sum(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (hex, path) = line.split_once("  ").or_else(|| line.split_once(" *"))?;
            Some((path.to_string(), hex.trim().to_ascii_lowercase()))
        })
        .collect()
}

/// The [`PROFILE_CONTROLS`] whose host file under `codex_home` is missing or
/// hashes differently from the container's copy in `seen`.
#[must_use]
pub fn drifted(codex_home: &Path, seen: &BTreeMap<String, String>) -> Vec<String> {
    PROFILE_CONTROLS
        .iter()
        .filter(|name| {
            let host = std::fs::read(codex_home.join(name))
                .ok()
                .map(|bytes| sha256_hex(&bytes));
            host.is_none() || seen.get(&profile_control_destination(name)) != host.as_ref()
        })
        .map(|name| (*name).to_string())
        .collect()
}

fn control_drift(docker: &str, container: &str, codex_home: Option<&Path>) -> ControlDrift {
    let codex_home = codex_home.ok_or("no --codex-home was given")?;
    let paths: Vec<String> = PROFILE_CONTROLS
        .iter()
        .map(|name| profile_control_destination(name))
        .collect();
    let output = Command::new(docker)
        .args(["exec", container, "sha256sum", "--"])
        .args(&paths)
        .output()
        .map_err(|error| format!("docker exec {container} sha256sum: {error}"))?;
    Ok(drifted(codex_home, &parse_sha256sum(&String::from_utf8_lossy(&output.stdout))))
}

/// Classify one `docker inspect` object.
#[must_use]
pub fn classify(state: &Value) -> Posture {
    // A crash-looping `--restart unless-stopped` container reads
    // `Running=true, Restarting=true` between restarts; nothing can be exec'd
    // into it, so it is not running (issue #10453).
    if state["State"]["Running"] != Value::Bool(true)
        || state["State"]["Restarting"] == Value::Bool(true)
    {
        return Posture::NotRunning;
    }
    let labels = &state["Config"]["Labels"];
    let private = labels["loom.workspace-mode"] == "private-clone";
    if !private && labels[SESSION_POSTURE_LABEL] != SESSION_POSTURE {
        return Posture::Unhardened;
    }
    let found = violations(state);
    if !found.is_empty() {
        Posture::Mismatch(found)
    } else if private {
        Posture::PrivateClone
    } else {
        Posture::Host
    }
}

/// Whether `dir` is inside one of `state`'s mount destinations.
#[must_use]
pub fn mounted(state: &Value, dir: &str) -> bool {
    let dir = std::path::Path::new(dir);
    state["Mounts"].as_array().is_some_and(|mounts| {
        mounts
            .iter()
            .filter_map(|m| m["Destination"].as_str())
            .any(|dest| !dest.is_empty() && dir.starts_with(dest))
    })
}

/// The decision `spawn-codex.sh` acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// `mode=` value.
    pub mode: String,
    /// `sandbox=` value: the mode Codex is launched with.
    pub sandbox: String,
    /// Whether to pass `GH_CONFIG_DIR` through into the container.
    pub forward_gh: bool,
    /// Lines for stderr.
    pub messages: Vec<String>,
    /// Exit code: 0, or 78 for a refusal.
    pub code: i32,
}

/// Environment the decision reads (injected so tests need not touch the
/// process environment).
#[derive(Debug, Default, Clone)]
pub struct Env {
    /// `LOOM_CODEX_CONTAINER_SANDBOX`.
    pub container_sandbox: Option<String>,
    /// `GH_CONFIG_DIR` (a path, never a token).
    pub gh_config_dir: Option<String>,
    /// `LOOM_PRIVATE_LEASE_FD` is set.
    pub leased: bool,
}

/// Decide. `state` is `None` when docker could not be asked at all. `drift`
/// reads the container's view of the profile controls; it is called only for
/// a hardened host-mode container.
#[must_use]
pub fn decide(
    args: &PostureArgs,
    state: Option<&Value>,
    env: &Env,
    drift: impl FnOnce() -> ControlDrift,
) -> Decision {
    let c = &args.container;
    let mut d = Decision {
        mode: String::new(),
        sandbox: args.requested.clone(),
        forward_gh: false,
        messages: Vec::new(),
        code: 0,
    };
    let pref = env.container_sandbox.as_deref().unwrap_or("off");
    if !matches!(pref, "off" | "codex") {
        d.messages.push(format!(
            "Invalid LOOM_CODEX_CONTAINER_SANDBOX='{pref}'. Valid values: off (default), codex."
        ));
        d.code = 78;
        return d;
    }
    let posture = state.map_or(Posture::NotRunning, classify);
    d.mode = posture.mode().into();
    let recreate = [
        "Recreate it (this restarts the account's session container):".to_string(),
        format!("  loom-daemon accounts session stop {}", args.profile),
        format!(
            "  loom-daemon accounts session start {} --mount-workspace <checkout parent>",
            args.profile
        ),
    ];
    match (&posture, pref) {
        (_, "codex") => d.messages.push(format!(
            "LOOM_CODEX_CONTAINER_SANDBOX=codex keeps sandbox={} inside {c} — bubblewrap fails there unless the container was given a user-namespace-capable seccomp/AppArmor profile (issue #9979)",
            args.requested
        )),
        (Posture::NotRunning, _) => d.messages.push(format!(
            "session container {c} posture not verified (not-running) — sandbox left as requested; dispatch will be refused below"
        )),
        (Posture::Unhardened, _) => {
            d.messages.push(format!("Session container {c} was created before the container-boundary hardening (issue #9979)."));
            d.messages.push(format!("Codex runs with its own sandbox off inside a session container, so Loom only dispatches into a container created with the {SESSION_POSTURE_LABEL}={SESSION_POSTURE} label."));
            d.messages.extend(recreate);
            d.code = 78;
            return d;
        }
        (Posture::Mismatch(found), _) => {
            d.messages.push(format!(
                "Session container {c} carries a hardened-posture label, but its actual settings are not hardened: {} (issue #9979).",
                found.join(" ")
            ));
            d.messages.push("Codex runs with its own sandbox off inside a session container, so Loom refuses to dispatch into it.".into());
            d.messages.extend(recreate);
            d.code = 78;
            return d;
        }
        (Posture::PrivateClone | Posture::Host, _) => {
            d.sandbox = "danger-full-access".into();
            d.messages.push(format!(
                "container={c} posture={} — Codex sandbox off, the container is the boundary (issue #9979)",
                d.mode
            ));
        }
    }
    if posture == Posture::Host {
        let changed = match drift() {
            Ok(changed) if changed.is_empty() => None,
            Ok(changed) => Some(changed.join(" ")),
            Err(error) => Some(format!("unverifiable ({error})")),
        };
        if let Some(changed) = changed {
            d.messages.push(format!(
                "Session container {c} does not see the host's copy of its profile control files: {changed}."
            ));
            d.messages.push("They are bound read-only, and a file bind does not follow a host-side replace (hook provisioning, accepting hook trust), so Codex inside would read a stale or missing registration while the host reads it as ready (issue #9979).".into());
            d.messages.extend(recreate);
            d.sandbox.clone_from(&args.requested);
            d.code = 78;
            return d;
        }
    }
    // GH_CONFIG_DIR: the daemon's App-token dir, mounted read-only into a
    // host-mode container at path parity; forwarding the PATH lets `gh`
    // inside authenticate as the fleet App. Never into a private clone (it
    // carries its own) or a leased private launch; and into a host-mode
    // container only when the path is actually mounted there.
    if let Some(dir) = env.gh_config_dir.as_deref().filter(|dir| !dir.is_empty()) {
        if posture != Posture::PrivateClone && !env.leased {
            d.forward_gh = posture != Posture::Host || state.is_some_and(|s| mounted(s, dir));
            if !d.forward_gh {
                d.messages.push(format!(
                    "GH_CONFIG_DIR is not mounted in {c} — gh inside the session will be unauthenticated. Recreate the session with `accounts session start` run from the daemon's root (its `--workspace`, or with LOOM_WORKSPACE set to it) so its App-token dir is mounted (issues #9979, #10103)."
                ));
            }
        }
    }
    d
}

/// `docker inspect <container>`'s first object, or `Ok(None)` when docker
/// could not be run at all. A container docker does not know, or output that
/// is not an inspect object, reads as not running (sandbox left on).
fn inspect(docker: &str, container: &str) -> Option<Value> {
    let output = Command::new(docker)
        .args(["inspect", "--type", "container", container])
        .output()
        .ok()?;
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    Some(match parsed {
        Value::Array(mut items) if output.status.success() && !items.is_empty() => {
            items.swap_remove(0)
        }
        _ => Value::Null,
    })
}

/// Entry point.
pub fn run(args: &PostureArgs) -> i32 {
    let env = Env {
        container_sandbox: std::env::var("LOOM_CODEX_CONTAINER_SANDBOX")
            .ok()
            .filter(|v| !v.is_empty()),
        gh_config_dir: std::env::var("GH_CONFIG_DIR").ok(),
        leased: std::env::var_os("LOOM_PRIVATE_LEASE_FD").is_some(),
    };
    let state = inspect(&args.docker, &args.container);
    let d = decide(args, state.as_ref(), &env, || {
        control_drift(&args.docker, &args.container, args.codex_home.as_deref())
    });
    for line in &d.messages {
        eprintln!("session-exec posture: {line}");
    }
    if d.code == 0 {
        println!(
            "mode={} sandbox={} gh={}",
            d.mode,
            d.sandbox,
            if d.forward_gh { "forward" } else { "skip" }
        );
    }
    d.code
}

#[cfg(test)]
#[path = "posture_tests.rs"]
mod tests;
