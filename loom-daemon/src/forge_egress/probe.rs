//! Production observation: builds the [`Observed`] snapshot the pure checks
//! read. Local and cheap by construction — `gh --version`, a few file reads
//! and one `git config --get-regexp`. The only network-capable step is the
//! negative canary, and only `doctor` runs it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::checks::{
    version_tuple, ApiHost, CanaryOutcome, GhBuild, Observed, Profile, ProfileSource,
};
use super::policy::{dig_str, PolicyDoc};

/// Upper bound on the canary run; the canary itself should carry its own
/// tighter `--max-time`.
const CANARY_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on `gh --version`.
const GH_VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Probe options.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeOptions {
    /// Run `enforcement.negativeCanary` (doctor only).
    pub run_canary: bool,
}

/// The `gh` Loom will actually exec: the program the spawn choke point's
/// resolver picks ([`crate::gh_invocation::resolver::resolve`] —
/// `$LOOM_GH_BIN`, else `gh`), resolved on `PATH` when bare. Not
/// `command -v gh` in some other shell. Single-sourced so the validator and
/// every facade spawn agree by construction.
#[must_use]
pub fn effective_gh_path() -> Option<PathBuf> {
    let name = PathBuf::from(crate::gh_invocation::resolver::resolve().program);
    if name.as_os_str().is_empty() {
        return None;
    }
    if name.components().count() > 1 {
        return name.is_file().then_some(name);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(&name))
            .find(|candidate| candidate.is_file())
    })
}

/// Token variables `gh` reads; stripped from the `--version` probe.
const GH_TOKEN_VARS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// `<path> --version`, first line only.
///
/// Run under an **empty** `GH_CONFIG_DIR` with every token variable removed:
/// under an ambient profile whose `hosts.yml` lacks a `user`, `gh` resolves
/// the login with a live GraphQL call even for `--version` (found by 2am#1929
/// P2) — so the validator's own probe would itself go direct.
#[must_use]
pub fn gh_build(path: Option<PathBuf>) -> GhBuild {
    let Some(exe) = path else {
        return GhBuild::default();
    };
    let empty_config = tempfile::tempdir().ok();
    let mut cmd = gh_version_command(&exe, empty_config.as_ref().map(tempfile::TempDir::path));
    let raw = run_bounded(&mut cmd, GH_VERSION_TIMEOUT)
        .map(|(_, out)| out.lines().next().unwrap_or("").trim().to_string())
        .unwrap_or_default();
    GhBuild {
        version: version_tuple(&raw),
        path: Some(exe),
        raw,
    }
}

/// The `gh --version` probe command: tokens stripped, `GH_CONFIG_DIR` pointed
/// at `empty_config` (or unset when no temp dir could be made).
fn gh_version_command(exe: &Path, empty_config: Option<&Path>) -> Command {
    let mut cmd = Command::new(exe);
    cmd.arg("--version");
    for var in GH_TOKEN_VARS {
        cmd.env_remove(var);
    }
    match empty_config {
        Some(dir) => cmd.env("GH_CONFIG_DIR", dir),
        None => cmd.env_remove("GH_CONFIG_DIR"),
    };
    cmd
}

/// Run `cmd` with a timeout; `Some((success, stdout))` when it finished.
///
/// Stdout is drained on a reader thread while this thread polls for exit, so
/// a child writing more than one pipe buffer (~64 KiB) is not mistaken for a
/// hang. The child runs in its own process group; on timeout the whole group
/// is killed (so a grandchild such as `curl` under `sh -c` cannot outlive the
/// probe or hold the pipe open) before the reader is joined.
fn run_bounded(cmd: &mut Command, timeout: Duration) -> Option<(bool, String)> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let pgid = i32::try_from(child.id()).ok();
    let reader = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = s.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });
    let join = |reader: Option<std::thread::JoinHandle<String>>| {
        reader.and_then(|r| r.join().ok()).unwrap_or_default()
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A grandchild that kept the pipe open would block the join;
                // the direct child has exited, so reap its group first.
                kill_group(pgid);
                return Some((status.success(), join(reader)));
            }
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                kill_group(pgid);
                let _ = child.kill();
                let _ = child.wait();
                let _ = join(reader);
                return None;
            }
        }
    }
}

/// SIGKILL the process group led by `pgid` (the probe child).
fn kill_group(pgid: Option<i32>) {
    if let Some(pgid) = pgid.filter(|p| *p > 1) {
        // SAFETY: `kill(2)` with a negative pid signals a process group; no
        // memory is touched. ESRCH (already gone) is ignored.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// `api_host` of the `logical` host entry in `<dir>/hosts.yml`.
///
/// A deliberately tiny reader for gh's own two-level layout: a column-0
/// `<host>:` key, then indented child keys. Only the verdict is returned —
/// the file's other contents (tokens included) are never retained.
#[must_use]
pub fn read_api_host(dir: &Path, logical: &str) -> ApiHost {
    let Ok(text) = std::fs::read_to_string(dir.join("hosts.yml")) else {
        return ApiHost::NoHostsFile;
    };
    parse_api_host(&text, logical)
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(s)
}

/// See [`read_api_host`].
#[must_use]
pub fn parse_api_host(text: &str, logical: &str) -> ApiHost {
    let mut in_entry = false;
    let mut found_entry = false;
    let mut child_indent: Option<usize> = None;
    for line in text.lines() {
        let content = line.split(" #").next().unwrap_or("").trim_end();
        if content.trim().is_empty() || content.trim_start().starts_with('#') {
            continue;
        }
        let indent = content.len() - content.trim_start().len();
        if indent == 0 {
            let key = content.strip_suffix(':').map(unquote);
            in_entry = key == Some(logical);
            found_entry |= in_entry;
            child_indent = None;
            continue;
        }
        if !in_entry {
            continue;
        }
        let first = *child_indent.get_or_insert(indent);
        if indent != first {
            continue;
        }
        if let Some((k, v)) = content.trim_start().split_once(':') {
            if unquote(k) == "api_host" {
                let v = unquote(v);
                return if v.is_empty() {
                    ApiHost::Missing
                } else {
                    ApiHost::Present(v.to_string())
                };
            }
        }
    }
    if found_entry {
        ApiHost::Missing
    } else {
        ApiHost::NoHostEntry
    }
}

/// gh's default config dir when no `GH_CONFIG_DIR` is exported.
fn default_gh_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("gh"));
    }
    dirs::home_dir().map(|h| h.join(".config").join("gh"))
}

/// Every profile directory Loom itself publishes for `workspace`: the
/// primary `.loom/gh-config` and each `.loom/gh-config-by-owner/<owner>`
/// (`credential_preflight`), when present on disk.
#[must_use]
pub fn loom_owned_profile_dirs(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let primary = crate::credential_preflight::github_app_gh_config_dir(workspace);
    if primary.is_dir() {
        out.push(primary);
    }
    let by_owner = crate::credential_preflight::github_app_gh_config_dir_for_owner(workspace, "x")
        .parent()
        .map(Path::to_path_buf);
    if let Some(Ok(entries)) = by_owner.map(std::fs::read_dir) {
        let mut owners: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        owners.sort();
        out.extend(owners);
    }
    out
}

/// Count `insteadOf`/`pushInsteadOf` rewrites whose matched prefix names the
/// logical host. Counts only — never URLs (they can embed credentials).
fn git_rewrites(cwd: &Path, logical: &str) -> usize {
    if logical.is_empty() {
        return 0;
    }
    let mut cmd = Command::new("git");
    cmd.args([
        "config",
        "--get-regexp",
        r"^url\..*\.(insteadof|pushinsteadof)$",
    ])
    .current_dir(cwd);
    run_bounded(&mut cmd, Duration::from_secs(10))
        .map(|(_, out)| {
            out.lines()
                .filter(|l| l.split_once(' ').is_some_and(|(_, v)| v.contains(logical)))
                .count()
        })
        .unwrap_or(0)
}

fn run_canary(command: &str) -> CanaryOutcome {
    match run_bounded(Command::new("sh").args(["-c", command]), CANARY_TIMEOUT) {
        Some((true, _)) => CanaryOutcome::Open,
        Some((false, _)) => CanaryOutcome::Blocked,
        None => CanaryOutcome::NotRun("did not complete within the bound"),
    }
}

fn loom_otlp_exporter(workspace: &Path) -> bool {
    let config = crate::observability::read_config(workspace);
    crate::observability::resolve_enabled(&config)
        && crate::observability::resolve_exporters(&config)
            .iter()
            .any(|e| e.kind == crate::observability::ExporterKind::Otlp)
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Observe this process and `workspace` for `doc`.
#[must_use]
pub fn observe(doc: &PolicyDoc, workspace: &Path, opts: ProbeOptions) -> Observed {
    let policy: &Value = &doc.data;
    let logical = dig_str(policy, &["github", "logicalHost"]);
    let gh_config_dir = env_nonempty("GH_CONFIG_DIR").map(PathBuf::from);
    let mut profiles = Vec::new();
    match &gh_config_dir {
        Some(dir) => profiles.push(Profile {
            path: dir.clone(),
            source: ProfileSource::Env,
            api_host: read_api_host(dir, logical),
        }),
        None => {
            if let Some(dir) = default_gh_config_dir() {
                profiles.push(Profile {
                    api_host: read_api_host(&dir, logical),
                    path: dir,
                    source: ProfileSource::Default,
                });
            }
        }
    }
    for dir in loom_owned_profile_dirs(workspace) {
        if gh_config_dir.as_deref() != Some(dir.as_path()) {
            profiles.push(Profile {
                api_host: read_api_host(&dir, logical),
                path: dir,
                source: ProfileSource::LoomOwned,
            });
        }
    }
    let launcher = dig_str(policy, &["toolchain", "launcherPath"]);
    let canary_cmd = super::policy::dig(policy, &["enforcement", "negativeCanary"])
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty());
    let canary = match canary_cmd {
        Some(_) if !opts.run_canary => None,
        Some(_) if !doc.origin.may_run_canary() => {
            Some(CanaryOutcome::NotRun("a repo-origin policy may not name a command"))
        }
        Some(cmd) => Some(run_canary(cmd)),
        None => None,
    };
    Observed {
        gh_host: env_nonempty("GH_HOST"),
        gh_repo: env_nonempty("GH_REPO"),
        gh_config_dir,
        gh: gh_build(effective_gh_path()),
        launcher_exists: !launcher.is_empty() && Path::new(launcher).exists(),
        profiles,
        git_rewrites: git_rewrites(workspace, logical),
        canary,
        loom_otlp_exporter: loom_otlp_exporter(workspace),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn canary_printing_more_than_a_pipe_buffer_is_open_not_not_run() {
        let out = run_bounded(
            Command::new("sh").args(["-c", "head -c 200000 /dev/zero | tr '\\0' a"]),
            Duration::from_secs(20),
        )
        .expect("finished within the bound");
        assert!(out.0);
        assert_eq!(out.1.len(), 200_000);
        assert!(matches!(
            run_canary("head -c 200000 /dev/zero | tr '\\0' a"),
            CanaryOutcome::Open
        ));
    }

    #[test]
    fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 300 & echo $! > '{}'; wait", pid_file.display());
        let started = Instant::now();
        let result = run_bounded(Command::new("sh").args(["-c", &script]), Duration::from_secs(1));
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(30), "reader join hung");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(gone, "grandchild {pid} survived the timeout");
    }

    #[test]
    fn parses_api_host_from_gh_layout_and_never_keeps_the_token() {
        let full = "github.com:\n    oauth_token: fixture-token-not-a-secret\n    user: u\n    api_host: proxy.example\n";
        assert_eq!(parse_api_host(full, "github.com"), ApiHost::Present("proxy.example".into()));
        let token_only =
            "github.com:\n    oauth_token: fixture-token-not-a-secret\n    user: x-access-token\n";
        assert_eq!(parse_api_host(token_only, "github.com"), ApiHost::Missing);
        assert_eq!(
            parse_api_host("ghe.example:\n    api_host: x\n", "github.com"),
            ApiHost::NoHostEntry
        );
        let nested = "github.com:\n    users:\n        api_host:\n            oauth_token: t\n    git_protocol: https\n";
        assert_eq!(
            parse_api_host(nested, "github.com"),
            ApiHost::Missing,
            "a nested key is not the host's"
        );
        let quoted = "\"github.com\":\n  api_host: \"proxy.example\" # routed\n";
        assert_eq!(parse_api_host(quoted, "github.com"), ApiHost::Present("proxy.example".into()));
    }

    #[test]
    fn gh_version_probe_strips_tokens_and_uses_an_empty_config() {
        let empty = tempfile::tempdir().unwrap();
        let cmd = gh_version_command(Path::new("/x/gh"), Some(empty.path()));
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        for var in GH_TOKEN_VARS {
            assert_eq!(envs.get(std::ffi::OsStr::new(var)), Some(&None), "{var} not removed");
        }
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GH_CONFIG_DIR")),
            Some(&Some(empty.path().as_os_str()))
        );
        let fallback = gh_version_command(Path::new("/x/gh"), None);
        assert!(fallback
            .get_envs()
            .any(|(k, v)| k == "GH_CONFIG_DIR" && v.is_none()));
    }

    #[test]
    #[cfg(unix)]
    fn gh_build_reads_the_first_version_line() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\necho 'gh version 2.97.0 (2025-01-01)'\necho more\n")
            .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let build = gh_build(Some(gh));
        assert_eq!(build.version, Some((2, 97, 0)));
        assert_eq!(build.raw, "gh version 2.97.0 (2025-01-01)");
    }

    #[test]
    fn missing_hosts_file_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_api_host(dir.path(), "github.com"), ApiHost::NoHostsFile);
    }

    #[test]
    fn loom_owned_dirs_enumerate_primary_and_per_owner_profiles() {
        let ws = tempfile::tempdir().unwrap();
        assert!(loom_owned_profile_dirs(ws.path()).is_empty());
        let primary = crate::credential_preflight::github_app_gh_config_dir(ws.path());
        let owner =
            crate::credential_preflight::github_app_gh_config_dir_for_owner(ws.path(), "acme");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&owner).unwrap();
        assert_eq!(loom_owned_profile_dirs(ws.path()), vec![primary, owner]);
    }
}
