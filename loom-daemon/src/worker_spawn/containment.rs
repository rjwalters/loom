//! Native-harness per-sweep ephemeral containment (issue #8403, epic #6896
//! Phase 3).
//!
//! The Claude adapter gained a per-sweep ephemeral container in #7429/#7430 by
//! having `spawn-claude.sh` re-exec ITSELF inside a `docker run`. Native
//! harnesses (Pi, OpenCode — admitted by #8363/#8400) have no shell adapter to
//! do that in: `spawn-worker.sh` is a 15-line stub that execs
//! `loom-daemon spawn-worker`, and the launch decision lives in
//! [`super::run`]. This module is the native counterpart of that block — the
//! SAME mechanism (re-exec the dispatcher inside a `docker run` of a pinned
//! image, guarded by the `LOOM_SPAWN_CONTAINERIZED=1` recursion sentinel),
//! expressed where the native launch decision actually is.
//!
//! Three things it fixes beyond "run in a box" (issue #8403's own framing):
//!
//! 1. **Data-directory isolation.** OpenCode's `auth.json` and session store
//!    live under `$XDG_DATA_HOME/opencode` (`~/.local/share/opencode` when
//!    unset). Uncontained, `XDG_DATA_HOME` is not relocated per launch, so N
//!    concurrent native workers on one host share one session store and one
//!    `auth.json`. Every XDG base directory — plus `OPENCODE_CONFIG_DIR` and
//!    the Loom binding directory (`LOOM_NATIVE_TOOLS_DIR`) — is pointed at a
//!    per-launch path inside the container's own ephemeral writable layer, so
//!    two concurrent contained workers are disjoint twice over: different
//!    containers, and different paths within them.
//! 2. **Containment.** Resource limits, a read-only view of everything outside
//!    the parity-mounted workspace, and a hard teardown at sweep end, all
//!    inherited from #7429/#7430 rather than reinvented.
//! 3. **Credential shape.** An API-key subscription has no refresh chain, so
//!    the Codex per-account SESSION container (whose whole reason to exist is
//!    that `CODEX_HOME/auth.json` is a mutable refresh chain needing one owning
//!    process — ADR-0017 Decision 1) buys nothing here. The key is forwarded
//!    into the container by NAME only (`-e VAR`, never `-e VAR=value`), so it
//!    appears in the container's process environment and nowhere in its argv,
//!    its writable layer, or any mounted path.

use super::LaunchError;
use serde_json::Value;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

/// The `containment=` telemetry token this profile writes to the per-sweep
/// log's `# LOOM_DISPATCH_MODE` marker — the field that distinguishes a
/// native-harness ephemeral container from Claude's (`claude-ephemeral`,
/// written by `spawn-claude.sh`). Parsed by
/// `crate::sweep_registry::containment_signal`.
pub const KIND: &str = "native-ephemeral";

/// Default image: the `loom-worker-native` overlay (`docker/native/`), which
/// is `loom-worker` plus the pinned OpenCode/Pi CLIs at the versions
/// `guardrail-parity-native.md` records as tested. Deliberately NOT the base
/// `loom-worker` image — that one ships no Node and therefore neither CLI, so
/// a dispatch into it would fail at `exec` with a bare "not found".
pub const DEFAULT_IMAGE: &str = "ghcr.io/rjwalters/loom-worker-native:latest";

/// The container-side home directory. The image's non-root `loom` user
/// (uid/gid 1000, `docker/worker/Dockerfile`) owns this path. Unlike the
/// workspace — whose absolute path is load-bearing and therefore parity-mounted
/// (`MOUNT-CONTRACT.md` §1) — HOME is not: nothing resolves a repo through it,
/// so the container keeps its own clean home rather than inheriting the host's.
const CONTAINER_HOME: &str = "/home/loom";

/// Fail-safe host memory total (MiB) when neither `/proc/meminfo` nor
/// `sysctl hw.memsize` can be read — mirrors `lib/memory-budget.sh`'s own
/// conservative 4 GiB fallback rather than inventing a second number.
const MEM_TOTAL_FALLBACK_MB: u64 = 4096;
/// Floor for a computed per-sweep memory share, mirroring `lib/memory-budget.sh`.
const MEM_FLOOR_MB: u64 = 512;
/// Default host memory held back before dividing the remainder across
/// in-flight sweeps — same default as `runtimes.containment.reservedMemoryMb`.
const MEM_RESERVED_DEFAULT_MB: u64 = 2048;

/// A resolved native-ephemeral launch profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub image: String,
    /// `docker run --cpus` value, or `None` for an intentionally-unbounded axis.
    pub cpus: Option<String>,
    /// `docker run --memory` value. Always `Some` in practice — an
    /// unconfigured memory cap is exactly the runaway-sweep gap #7430 closed.
    pub memory: Option<String>,
    /// Per-launch directory name under `CONTAINER_HOME/.loom-native/`, so the
    /// two concurrent-worker filesystems differ by PATH as well as by
    /// container — an assertion a human can make by inspection.
    pub launch_id: String,
}

impl Profile {
    /// The per-launch ephemeral root inside the container's writable layer.
    fn ephemeral_root(&self) -> String {
        format!("{CONTAINER_HOME}/.loom-native/{}", self.launch_id)
    }

    /// The canonical marker line the OUTER (host-side) dispatch writes to the
    /// per-sweep log, matching `spawn-claude.sh`'s field order exactly so one
    /// parser serves both. `none` (never an empty field) marks an
    /// intentionally-unbounded axis.
    pub fn dispatch_marker(&self) -> String {
        format!(
            "# LOOM_DISPATCH_MODE mode=container image={} cpus={} memory={} containment={KIND}",
            self.image,
            self.cpus.as_deref().unwrap_or("none"),
            self.memory.as_deref().unwrap_or("none"),
        )
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn truthy(value: &str) -> bool {
    matches!(value.trim(), "1" | "true" | "yes" | "ephemeral")
}

fn config_str(config: &Value, pointer: &str) -> Option<String> {
    match config.pointer(pointer)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Is native-harness containment turned on for this workspace?
///
/// Precedence (env > config > default-off), mirroring every other Loom
/// toggle. Deliberately a SEPARATE switch from `runtimes.containment.enabled`
/// (Claude's, #7429): that flag selects the `loom-worker` image, which ships
/// no native CLI, and its fleet-default flip is soak-gated on Claude's own
/// evidence (#7431/#7767). Inheriting it would silently dispatch native
/// sweeps into an image that cannot run them.
///
/// | Precedence | Source |
/// |---|---|
/// | 1 | `LOOM_NATIVE_CONTAINERIZED` (`1`/`true`/`yes`/`ephemeral` enables; anything else disables) |
/// | 2 | `.loom/config.json` → `runtimes.containment.native` (`"ephemeral"` / `true`) |
/// | 3 | off — byte-for-byte uncontained native dispatch, unchanged |
pub fn enabled(config: &Value) -> bool {
    if let Some(raw) = env_nonempty("LOOM_NATIVE_CONTAINERIZED") {
        return truthy(&raw);
    }
    config_str(config, "/runtimes/containment/native").is_some_and(|v| truthy(&v))
}

/// Resolve the launch profile, or `None` when this dispatch must stay
/// uncontained — either containment is off, or this process is ALREADY the
/// re-exec'd copy running inside the container (`LOOM_SPAWN_CONTAINERIZED=1`,
/// the same recursion sentinel `spawn-claude.sh` uses, so the two dispatch
/// paths cannot disagree about what "inside" means).
pub fn resolve(config: &Value) -> Option<Profile> {
    if env_nonempty("LOOM_SPAWN_CONTAINERIZED").is_some() {
        return None;
    }
    if !enabled(config) {
        return None;
    }
    Some(Profile {
        image: env_nonempty("LOOM_NATIVE_CONTAINER_IMAGE")
            .or_else(|| config_str(config, "/runtimes/containment/nativeImage"))
            .unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
        cpus: resolve_cpus(config),
        memory: resolve_memory(config),
        launch_id: uuid::Uuid::new_v4().simple().to_string(),
    })
}

/// `--cpus`: explicit env → config → the SAME host-wide budget the bare-metal
/// path already computed (`LOOM_SWEEP_CPU_BUDGET_CORES`, #5111/#5979) → no
/// flag at all. An empty budget means the CPU-quota mechanism was deliberately
/// disabled, so containment applies no cap either rather than inventing one.
fn resolve_cpus(config: &Value) -> Option<String> {
    env_nonempty("LOOM_SWEEP_CONTAINER_CPUS")
        .or_else(|| config_str(config, "/runtimes/containment/cpus"))
        .or_else(|| env_nonempty("LOOM_SWEEP_CPU_BUDGET_CORES"))
}

/// `--memory`: explicit env → config → a computed host-wide share, divided
/// across in-flight sweeps exactly the way `lib/memory-budget.sh` does for the
/// Claude path. Always applied: an unconfigured memory cap is the
/// "one runaway sweep starves the host" gap #7430 exists to close.
fn resolve_memory(config: &Value) -> Option<String> {
    if let Some(explicit) = env_nonempty("LOOM_SWEEP_CONTAINER_MEMORY")
        .or_else(|| config_str(config, "/runtimes/containment/memory"))
    {
        return Some(explicit);
    }
    let reserved = env_nonempty("LOOM_SWEEP_CONTAINER_RESERVED_MEMORY_MB")
        .or_else(|| config_str(config, "/runtimes/containment/reservedMemoryMb"))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(MEM_RESERVED_DEFAULT_MB);
    let in_flight = env_nonempty("LOOM_SWEEP_INFLIGHT_SWEEPS")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(1);
    Some(format!("{}m", budget_mb(host_total_mb(), reserved, in_flight)))
}

/// Rust mirror of `lib/memory-budget.sh`'s `loom_mem_budget_mb`:
/// `max(512, floor(max(512, total - reserved) / in_flight))`.
fn budget_mb(total_mb: u64, reserved_mb: u64, in_flight: u64) -> u64 {
    let in_flight = in_flight.max(1);
    let usable = total_mb.saturating_sub(reserved_mb).max(MEM_FLOOR_MB);
    (usable / in_flight).max(MEM_FLOOR_MB)
}

/// Rust mirror of `lib/memory-budget.sh`'s `loom_mem_total_mb`. Never returns
/// 0: every caller does budget arithmetic on the result.
fn host_total_mb() -> u64 {
    if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = parse_meminfo_total_kb(&contents) {
            return (kb / 1024).max(MEM_FLOOR_MB);
        }
    }
    if let Ok(crate::proc_exec::Completion::Exited(out)) = crate::proc_exec::run_bounded(
        {
            let mut c = Command::new("sysctl");
            c.args(["-n", "hw.memsize"]);
            c
        },
        std::time::Duration::from_secs(5),
    ) {
        if let Some(bytes) = std::str::from_utf8(&out.stdout)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|b| *b > 0)
        {
            return (bytes / 1024 / 1024).max(MEM_FLOOR_MB);
        }
    }
    MEM_TOTAL_FALLBACK_MB
}

fn parse_meminfo_total_kb(contents: &str) -> Option<u64> {
    contents
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .filter(|kb| *kb > 0)
}

/// Env vars that are meaningful on the HOST but actively wrong inside the
/// container: every one of them names a host filesystem path that either does
/// not exist in the image or points at the host's own binaries. Forwarding
/// them would make the contained dispatch fail in a way that looks like a
/// missing CLI rather than a leaked host path.
const HOST_ONLY_ENV: &[&str] = &[
    "LOOM_PI_BIN",
    "LOOM_OPENCODE_BIN",
    "LOOM_DAEMON_BIN",
    "LOOM_DAEMON_SELF_BIN",
    "LOOM_NATIVE_TOOL_BIN",
    "LOOM_NATIVE_TOOLS_DIR",
    "LOOM_NATIVE_GUARD_DIR",
    "LOOM_DAEMON_LOG",
    "LOOM_SPAWN_CONTAINERIZED",
    "LOOM_WORKSPACE",
];

fn forwarded_by_name(name: &str) -> bool {
    if HOST_ONLY_ENV.contains(&name) {
        return false;
    }
    name.starts_with("LOOM_")
        || name.starts_with("SAFEHOUSE")
        || matches!(name, "GH_TOKEN" | "GITHUB_TOKEN" | "NO_COLOR" | "TERM")
}

/// Build the `docker run` command that re-execs `spawn-worker.sh` inside the
/// per-sweep ephemeral container with `args` unchanged.
///
/// `credentials` are env var NAMES (the selected model profile's
/// `credentialEnv` and its per-harness `credentialTargets` entry). They are
/// forwarded with `-e NAME` and no `=value`, so docker reads the current value
/// straight from this process's environment: the key never appears in this
/// command's argv (and therefore never in `ps`, a shell history, or a log),
/// and nothing writes it to a file anywhere in the container.
pub fn docker_command(
    profile: &Profile,
    workspace: &Path,
    cwd: &Path,
    log: Option<&Path>,
    args: &[OsString],
    credentials: &[&str],
) -> Result<Command, LaunchError> {
    if which_docker().is_none() {
        return Err(LaunchError::config(
            "native containment is enabled (runtimes.containment.native / LOOM_NATIVE_CONTAINERIZED) but 'docker' is not on PATH. Install docker, or disable containment (LOOM_NATIVE_CONTAINERIZED=0, or remove runtimes.containment.native from .loom/config.json).",
        ));
    }
    let root = profile.ephemeral_root();
    let mut command = Command::new("docker");
    command.arg("run").arg("--rm");

    // --- Mounts (MOUNT-CONTRACT.md) -------------------------------------
    // §1 path parity: the workspace is bind-mounted read-write at the
    // IDENTICAL absolute host path, so git's absolute worktree pointers
    // (`.git` gitdir files, `commondir`) resolve identically inside and out.
    let workspace_spec = workspace.display().to_string();
    command
        .arg("-v")
        .arg(mount(workspace, &workspace_spec, false));
    // Everything ELSE on the host is simply absent from the container — the
    // "read-only view of everything outside the worktree" the issue asks for
    // is the container boundary itself, not a flag.
    for (host, container, read_only) in extra_mounts(&root, log, workspace) {
        command.arg("-v").arg(mount(&host, &container, read_only));
    }
    command.arg("-w").arg(cwd);

    // --- Per-launch isolated XDG / config / binding directories ----------
    // All under the container's own ephemeral writable layer, never a mounted
    // host path, so nothing here survives the container and two concurrent
    // workers cannot see each other's session store or `auth.json`.
    for (key, sub) in [
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("OPENCODE_CONFIG_DIR", "opencode"),
        ("LOOM_NATIVE_TOOLS_DIR", "native-tools"),
    ] {
        command.arg("-e").arg(format!("{key}={root}/{sub}"));
    }
    // Never self-update past the version this image pins and
    // guardrail-parity-native.md records as tested.
    command.arg("-e").arg("OPENCODE_DISABLE_AUTOUPDATE=1");
    command.arg("-e").arg(format!("HOME={CONTAINER_HOME}"));
    command
        .arg("-e")
        .arg(format!("LOOM_WORKSPACE={}", workspace.display()));
    command.arg("-e").arg("LOOM_SPAWN_CONTAINERIZED=1");
    command
        .arg("-e")
        .arg(format!("LOOM_NATIVE_CONTAINMENT={KIND}"));

    // --- Env passthrough, BY NAME ----------------------------------------
    let mut names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| forwarded_by_name(k))
        .collect();
    names.extend(
        credentials
            .iter()
            .filter(|n| !n.is_empty() && std::env::var_os(n).is_some())
            .map(|n| (*n).to_string()),
    );
    names.sort();
    names.dedup();
    for name in names {
        command.arg("-e").arg(name);
    }

    // --- Limits + observability labels (issue #7430's shape) -------------
    if let Some(cpus) = &profile.cpus {
        command.arg("--cpus").arg(cpus);
    }
    if let Some(memory) = &profile.memory {
        command.arg("--memory").arg(memory);
    }
    command.arg("--label").arg("loom.sweep=1");
    command.arg("--label").arg("loom.dispatch=container");
    command
        .arg("--label")
        .arg(format!("loom.containment={KIND}"));
    if let Some(issue) = env_nonempty("LOOM_SWEEP_CLAIM_OWNED") {
        command
            .arg("--label")
            .arg(format!("loom.sweep.issue={issue}"));
    }
    if let Some(cpus) = &profile.cpus {
        command
            .arg("--label")
            .arg(format!("loom.dispatch.cpus={cpus}"));
    }
    if let Some(memory) = &profile.memory {
        command
            .arg("--label")
            .arg(format!("loom.dispatch.memory={memory}"));
    }

    command.arg(&profile.image);
    command.arg(workspace.join(".loom/scripts/spawn-worker.sh"));
    command.args(args);
    Ok(command)
}

fn which_docker() -> Option<PathBuf> {
    if env_nonempty("LOOM_TEST_ASSUME_DOCKER").is_some() {
        return Some(PathBuf::from("docker"));
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("docker"))
            .find(|candidate| candidate.is_file())
    })
}

fn mount(host: &Path, container: &str, read_only: bool) -> OsString {
    let mut spec = OsString::from(host);
    spec.push(":");
    spec.push(container);
    if read_only {
        spec.push(":ro");
    }
    spec
}

/// Best-effort host mounts beyond the workspace: forge/git identity remapped
/// under the CONTAINER's home (HOME is not path-parity-load-bearing, unlike
/// the workspace), the log directory when it lives outside the workspace, and
/// the cargo build cache when it does. `gh` config lands inside the redirected
/// `XDG_CONFIG_HOME` on purpose — `gh` honours XDG, so mounting it at the
/// host's `~/.config/gh` would be invisible to it.
fn extra_mounts(
    ephemeral_root: &str,
    log: Option<&Path>,
    workspace: &Path,
) -> Vec<(PathBuf, String, bool)> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        let gitconfig = home.join(".gitconfig");
        if gitconfig.is_file() {
            out.push((gitconfig, format!("{CONTAINER_HOME}/.gitconfig"), true));
        }
        let gh = home.join(".config/gh");
        if env_nonempty("GH_TOKEN").is_none()
            && env_nonempty("GITHUB_TOKEN").is_none()
            && gh.is_dir()
        {
            out.push((gh, format!("{ephemeral_root}/config/gh"), true));
        }
    }
    if let Some(dir) = log
        .and_then(Path::parent)
        .filter(|p| !p.as_os_str().is_empty())
    {
        if !dir.starts_with(workspace) && dir.is_dir() {
            out.push((dir.to_path_buf(), dir.display().to_string(), false));
        }
    }
    out
}

#[cfg(test)]
#[path = "containment_tests.rs"]
mod tests;
