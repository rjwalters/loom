//! What a policy-governed host puts in front of every worker's `gh` (#9987,
//! C4 of epic #9983).
//!
//! Loom does not build or provision the managed `gh` launcher (2am's
//! `scripts/gh-managed.py`); it consumes `toolchain.launcherPath` and makes
//! that launcher the `gh` every worker actually runs:
//!
//! - **bare metal** — [`WorkerEgress::worker_path`] puts the launcher's
//!   directory first on the worker `PATH`;
//! - **containers** — [`WorkerEgress::container_mounts`] /
//!   [`WorkerEgress::container_env`] carry the launcher, the pinned upstream
//!   `gh`, the policy and the credential *reference* file into the container
//!   read-only at their host paths (the containerised copy of `spawn-worker`
//!   then applies the same `PATH` rule), and the caller stops mounting
//!   `~/.config/gh` and forwarding `GH_TOKEN` / `GITHUB_TOKEN`;
//! - **spawn gate** — [`WorkerEgress::launcher_first_finding`] refuses a
//!   worker whose first `gh` is not the launcher (`toolchain.launcher-not-first`).
//!
//! No policy (or a repo-origin one, which may not choose an executable) means
//! [`WorkerEgress::from_sources`] is `None` and every caller is byte-identical
//! to its pre-#9987 behaviour.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::policy::{self, dig_str, PolicySources, Resolution};
use super::report::Finding;

/// Env var the launcher reads its policy from, besides [`policy::POLICY_ENV`].
pub const LAUNCHER_POLICY_ENV: &str = "GITHUB_EGRESS_POLICY";

/// The launcher, policy and credential reference of a resolved policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerEgress {
    pub launcher: PathBuf,
    pub upstream_gh: Option<PathBuf>,
    pub policy_file: PathBuf,
    /// The file named by `principal.credentialRef` (`file:/abs/path`); other
    /// reference schemes are not mountable and are left to the launcher.
    pub credential_file: Option<PathBuf>,
    /// `enforcement.api = required`.
    pub required: bool,
}

impl WorkerEgress {
    /// Resolve from injected `sources` (hermetic tests never touch `/etc`).
    /// `Some` only for a loaded env/machine policy whose
    /// `toolchain.launcherPath` is an existing absolute path.
    #[must_use]
    pub fn from_sources(sources: &PolicySources) -> Option<Self> {
        let Resolution::Loaded(doc) = policy::resolve(sources) else {
            return None;
        };
        if !doc.origin.may_choose_executable() {
            return None;
        }
        let launcher = Path::new(dig_str(&doc.data, &["toolchain", "launcherPath"]));
        if !launcher.is_absolute() || !launcher.exists() {
            return None;
        }
        let upstream = dig_str(&doc.data, &["toolchain", "upstreamGhPath"]);
        let credential = dig_str(&doc.data, &["principal", "credentialRef"])
            .strip_prefix("file:")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        Some(Self {
            launcher: launcher.to_path_buf(),
            upstream_gh: Some(PathBuf::from(upstream)).filter(|p| p.is_absolute()),
            policy_file: doc.path.clone(),
            credential_file: credential,
            required: dig_str(&doc.data, &["enforcement", "api"]) == "required",
        })
    }

    /// Resolve from the live process. Off in a unit-test build and under
    /// [`crate::gh_invocation::resolver::NO_POLICY_LAUNCHER_ENV`], like the
    /// resolver's policy rung, so a policy on the test host cannot alter a
    /// golden spawn env.
    #[must_use]
    pub fn from_process() -> Option<Self> {
        use crate::gh_invocation::resolver::{policy_rung_declined, NO_POLICY_LAUNCHER_ENV};
        if cfg!(test) || policy_rung_declined(std::env::var_os(NO_POLICY_LAUNCHER_ENV).as_deref()) {
            return None;
        }
        Self::from_sources(&PolicySources::from_process(None))
    }

    #[must_use]
    pub fn launcher_dir(&self) -> Option<&Path> {
        self.launcher.parent().filter(|p| !p.as_os_str().is_empty())
    }

    /// `current` with the launcher's directory first (and not repeated).
    #[must_use]
    pub fn worker_path(&self, current: Option<&OsStr>) -> Option<OsString> {
        crate::agent_gh::prepend_path(self.launcher_dir()?, current)
    }

    /// Read-only same-path mounts for a container: `(host, container, ro)`.
    #[must_use]
    pub fn container_mounts(&self) -> Vec<(PathBuf, String, bool)> {
        let mut paths: Vec<&Path> = Vec::new();
        paths.extend(self.launcher_dir());
        paths.extend(self.upstream_gh.as_deref());
        paths.push(&self.policy_file);
        paths.extend(self.credential_file.as_deref());
        let mut out: Vec<(PathBuf, String, bool)> = Vec::new();
        for p in paths {
            // A file already under a mounted directory needs no second mount.
            if out.iter().any(|(d, _, _)| p != d && p.starts_with(d)) || !p.exists() {
                continue;
            }
            out.push((p.to_path_buf(), p.display().to_string(), true));
        }
        out
    }

    /// `-e KEY=VALUE` pairs a container needs to find the same policy.
    #[must_use]
    pub fn container_env(&self) -> Vec<(&'static str, String)> {
        let policy = self.policy_file.display().to_string();
        vec![
            (policy::POLICY_ENV, policy.clone()),
            (LAUNCHER_POLICY_ENV, policy),
        ]
    }

    /// The `docker run` arguments for [`Self::container_mounts`] and
    /// [`Self::container_env`], one per element (`-v`, spec, `-e`, spec, …).
    /// What `forge egress container-args` prints for `spawn-claude.sh`.
    #[must_use]
    pub fn docker_args(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (host, container, _) in self.container_mounts() {
            out.push("-v".to_string());
            out.push(format!("{}:{container}:ro", host.display()));
        }
        for (key, value) in self.container_env() {
            out.push("-e".to_string());
            out.push(format!("{key}={value}"));
        }
        out
    }

    /// `toolchain.launcher-not-first` for a worker whose `PATH` is `path`.
    /// Only under `enforcement.api = required`; `observe` never refuses.
    #[must_use]
    pub fn launcher_first_finding(&self, path: Option<&OsStr>) -> Option<Finding> {
        if !self.required {
            return None;
        }
        let first = path.and_then(|p| {
            std::env::split_paths(p)
                .map(|d| d.join("gh"))
                .find(|c| c.is_file())
        });
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if first
            .as_deref()
            .is_some_and(|f| canon(f) == canon(&self.launcher))
        {
            return None;
        }
        Some(
            Finding::new(
                "toolchain.launcher-not-first",
                "the `gh` a worker resolves is the managed launcher",
            )
            .expected(self.launcher.display().to_string())
            .observed(first.map_or_else(|| "(none on PATH)".into(), |f| f.display().to_string()))
            .source("worker PATH")
            .remedy(
                "name the launcher `gh` in toolchain.launcherPath's directory; Loom prepends \
                 that directory to the worker PATH",
            ),
        )
    }
}

/// The refusal text for a spawn blocked by `finding`.
#[must_use]
pub fn refusal_message(finding: &Finding) -> String {
    format!(
        "forge-egress: worker spawn refused (enforcement.api=required): [{}] expected {}, \
         observed {} — {}",
        finding.code, finding.expected, finding.observed, finding.remedy
    )
}

/// `toolchain.launcher-python3-missing`: the launcher is Python 3, so a
/// container image without it cannot run a managed `gh`.
#[must_use]
pub fn python3_missing_finding(image: &str) -> Finding {
    Finding::new(
        "toolchain.launcher-python3-missing",
        "the worker image can run the managed (Python 3) gh launcher",
    )
    .expected("python3 on the image PATH")
    .observed("not found")
    .source(format!("docker image {image}"))
    .remedy("add python3 to the worker image (docker/worker/Dockerfile or the FROM layer)")
}

#[cfg(test)]
#[path = "worker_env_tests.rs"]
mod tests;
