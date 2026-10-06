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

/// The outcome of [`WorkerEgress::admit`] for a spawn that may proceed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admission {
    /// An env/machine policy was loaded. `false` is the only state in which a
    /// caller may keep the legacy `~/.config/gh` / `GH_TOKEN` credentials
    /// without logging why.
    pub configured: bool,
    /// The managed launcher to install; `None` with `configured` only under a
    /// valid `observe` policy whose launcher is unusable (see `warnings`).
    pub egress: Option<WorkerEgress>,
    /// `observe` findings: logged by the caller, never a refusal.
    pub warnings: Vec<Finding>,
}

impl Admission {
    /// The one-line status `forge egress container-args` prints first, so the
    /// shell never reads "no output" as "no policy": `unconfigured`,
    /// `managed` (the docker arguments follow), or `observe-unmanaged`
    /// (a valid observe policy whose launcher is unusable: logged, legacy).
    #[must_use]
    pub fn status(&self) -> &'static str {
        match (&self.egress, self.configured) {
            (Some(_), _) => "managed",
            (None, true) => "observe-unmanaged",
            (None, false) => "unconfigured",
        }
    }
}

impl WorkerEgress {
    /// Resolve from injected `sources` (hermetic tests never touch `/etc`).
    /// `Some` only for a loaded env/machine policy whose
    /// `toolchain.launcherPath` is an existing absolute path. Collapses every
    /// refusal into `None`; a caller that must not restore ambient credentials
    /// uses [`Self::try_from_sources`].
    #[must_use]
    pub fn from_sources(sources: &PolicySources) -> Option<Self> {
        Self::try_from_sources(sources).ok().flatten()
    }

    /// [`Self::from_sources`] that keeps a *configured* policy distinguishable
    /// from an absent one: [`Self::admit`] without its `observe` warnings.
    ///
    /// # Errors
    /// The [`Finding`] naming the policy or launcher failure.
    pub fn try_from_sources(sources: &PolicySources) -> Result<Option<Self>, Box<Finding>> {
        Self::admit(sources).map(|a| a.egress)
    }

    /// Resolve `sources` into an [`Admission`]. `Ok` with `configured: false`
    /// is only a genuinely unconfigured host (no policy, or a repo-origin one,
    /// which may not choose an executable). An env/machine policy is validated
    /// (schema version, schema, [`policy::assert_policy_shape`]) BEFORE any
    /// routing result is returned, and `Err` is a named refusal: an
    /// unreadable policy, an unsupported `schemaVersion` (fail closed, never
    /// observe-only), or — unless the policy is a valid `observe` one — any
    /// shape finding or a launcher that is not an existing absolute path.
    /// Under `observe` those are [`Admission::warnings`] and the spawn
    /// proceeds. Never fall back to host credentials on `Err` (#9987).
    ///
    /// # Errors
    /// The [`Finding`] naming the policy or launcher failure.
    pub fn admit(sources: &PolicySources) -> Result<Admission, Box<Finding>> {
        let doc = match policy::resolve(sources) {
            Resolution::Unconfigured if !sources.managed => return Ok(Admission::default()),
            Resolution::Unconfigured => {
                return Err(Box::new(
                    Finding::new(
                        "policy.unconfigured",
                        "a host declared managed has a forge egress policy",
                    )
                    .expected("a policy at the env or machine path")
                    .observed("none")
                    .source("managed marker")
                    .remedy("provision the policy, or remove the managed marker"),
                ));
            }
            Resolution::Unreadable {
                candidate, error, ..
            } => {
                return Err(Box::new(
                    Finding::new(
                        "policy.unreadable",
                        "the forge egress policy is readable and valid",
                    )
                    .expected("a readable policy with a known schemaVersion")
                    .observed(error)
                    .source(candidate.path.display().to_string())
                    .remedy(
                        "repair or remove the policy file; Loom will not guess a narrower policy",
                    ),
                ));
            }
            Resolution::Loaded(doc) => doc,
        };
        if !doc.origin.may_choose_executable() {
            return Ok(Admission::default());
        }
        // `false` for an unknown schemaVersion or a missing/out-of-enum
        // `enforcement.api`: those fail closed as `required`.
        let observe = policy::is_observe_only(&doc.data);
        let source = doc.path.display().to_string();
        let mut warnings = Vec::new();
        for mut finding in policy::assert_policy_shape(&doc.data) {
            finding.source = format!("{} ({source})", finding.source);
            if !observe {
                return Err(Box::new(finding));
            }
            warnings.push(finding);
        }
        let configured = dig_str(&doc.data, &["toolchain", "launcherPath"]);
        let launcher = Path::new(configured);
        if configured.is_empty() || !launcher.is_absolute() || !launcher.exists() {
            let finding = Finding::new(
                "toolchain.launcher-missing",
                "toolchain.launcherPath is an existing absolute path",
            )
            .expected("an existing absolute path")
            .observed(if configured.is_empty() {
                "(empty or absent)"
            } else {
                configured
            })
            .source(source)
            .remedy("provision the managed gh launcher at toolchain.launcherPath");
            if !observe {
                return Err(Box::new(finding));
            }
            warnings.push(finding);
            return Ok(Admission {
                configured: true,
                egress: None,
                warnings,
            });
        }
        let upstream = dig_str(&doc.data, &["toolchain", "upstreamGhPath"]);
        let credential = dig_str(&doc.data, &["principal", "credentialRef"])
            .strip_prefix("file:")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        Ok(Admission {
            configured: true,
            egress: Some(Self {
                launcher: launcher.to_path_buf(),
                upstream_gh: Some(PathBuf::from(upstream)).filter(|p| p.is_absolute()),
                policy_file: doc.path.clone(),
                credential_file: credential,
                required: !observe,
            }),
            warnings,
        })
    }

    /// Resolve from the live process. Off in a unit-test build and under
    /// [`crate::gh_invocation::resolver::NO_POLICY_LAUNCHER_ENV`], like the
    /// resolver's policy rung, so a policy on the test host cannot alter a
    /// golden spawn env.
    #[must_use]
    pub fn from_process() -> Option<Self> {
        Self::try_from_process().ok().flatten()
    }

    /// [`Self::from_process`] with [`Self::try_from_sources`]'s refusals.
    ///
    /// # Errors
    /// The [`Finding`] naming the policy or launcher failure.
    pub fn try_from_process() -> Result<Option<Self>, Box<Finding>> {
        Self::admit_process().map(|a| a.egress)
    }

    /// [`Self::admit`] against the live process (off, i.e. unconfigured, in a
    /// unit-test build and under the resolver's opt-out, like
    /// [`Self::from_process`]).
    ///
    /// # Errors
    /// The [`Finding`] naming the policy or launcher failure.
    pub fn admit_process() -> Result<Admission, Box<Finding>> {
        use crate::gh_invocation::resolver::{policy_rung_declined, NO_POLICY_LAUNCHER_ENV};
        if cfg!(test) || policy_rung_declined(std::env::var_os(NO_POLICY_LAUNCHER_ENV).as_deref()) {
            return Ok(Admission::default());
        }
        Self::admit(&PolicySources::from_process(None))
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

/// The log line for an `observe` finding that did not refuse the spawn.
#[must_use]
pub fn observe_message(finding: &Finding) -> String {
    format!(
        "forge-egress: observe (enforcement.api=observe, spawn proceeds): [{}] expected {}, \
         observed {} — {}",
        finding.code, finding.expected, finding.observed, finding.remedy
    )
}

/// `toolchain.launcher-not-first` for a container image: `resolved` is what
/// `command -v gh` printed inside it with the launcher directory first on
/// `PATH` and the policy mounts applied (`None` when the probe could not run,
/// which is not a verdict). Only under `enforcement.api = required`.
#[must_use]
pub fn container_launcher_finding(
    egress: &WorkerEgress,
    image: &str,
    resolved: Option<&str>,
) -> Option<Finding> {
    let resolved = resolved?;
    if !egress.required || Path::new(resolved) == egress.launcher {
        return None;
    }
    Some(
        Finding::new(
            "toolchain.launcher-not-first",
            "the `gh` a containerised worker resolves is the managed launcher",
        )
        .expected(egress.launcher.display().to_string())
        .observed(if resolved.is_empty() {
            "(none on PATH)"
        } else {
            resolved
        })
        .source(format!("docker image {image}"))
        .remedy(
            "name the launcher `gh` in toolchain.launcherPath's directory; it is mounted \
             read-only at its host path and put first on the container PATH",
        ),
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
