//! The provider-free readiness probe: the real pinned CLI, a hard deadline, no
//! credential in the child's environment, and no argv that could reach a model.
//!
//! # Why an allowlist rather than a denylist
//!
//! "Do not pass `run`" is a rule an operator flag can get around by accident —
//! a new inference verb in a future CLI major, an alias, a `--prompt` spelled
//! differently. [`READINESS_ALLOWLIST`] inverts that: the probe will execute
//! *only* tokens it already knows to be provider-free, and refuses anything
//! else without echoing it. The refusal is therefore the safety property, and
//! an unknown-but-harmless token costs an allowlist entry rather than a
//! silent model call.
//!
//! Two consequences worth stating plainly:
//!
//! * The report's `readiness_command` cannot carry a secret, because every
//!   token in it came from a compile-time constant list.
//! * A refusal message never contains the rejected token, so a mistyped flag
//!   holding a pasted key cannot be laundered into a log line.
//!
//! # What a successful probe does and does not prove
//!
//! It proves the binary executes, the guarded bindings can be written, the
//! pinned package set can be materialized, and the CLI answers a provider-free
//! invocation inside the deadline. It does **not** prove inference readiness,
//! and it does not prove the CLI loaded the guarded plugin — a help/config
//! command returning 0 is not evidence of plugin load. That is why
//! [`super::ReadinessReport::plugin_load_proven`] stays `None`.

use super::{Classification, NetworkMode, Observation};
use crate::proc_exec::{run_bounded, Completion, ExecError};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Every argv token this probe will execute. Provider-free by construction:
/// none of these accepts a prompt, selects a model, or issues a provider
/// request on any OpenCode major this repo supports.
///
/// Adding an entry is a deliberate act — it must be a subcommand or flag that
/// cannot reach a provider. `run`, `--prompt`, `-p`, `--model`, `--agent`,
/// `--auto` and `--variant` are absent on purpose and must stay absent.
pub const READINESS_ALLOWLIST: &[&str] = &[
    "--version",
    "-v",
    "--help",
    "-h",
    "help",
    "debug",
    "config",
    "info",
    "models",
    "--print-logs",
];

/// Argv used when the caller names none: the cheapest provider-free answer the
/// CLI can give.
pub const DEFAULT_READINESS: &[&str] = &["--version"];

/// Package manager used to materialize the pinned plugin set by default.
pub const DEFAULT_PACKAGE_MANAGER: &str = "npm";

/// Environment variables inherited from the parent. `PATH` only: the harness is
/// a Node/Bun program that must be able to find its own interpreter, and
/// nothing else about the parent's environment is needed to answer a
/// provider-free question.
const INHERITED: &[&str] = &["PATH"];

/// Substrings that make an environment variable name credential-shaped.
///
/// Matched case-insensitively against the whole name, so this catches
/// `ZAI_API_KEY`, `GH_TOKEN`, `GITHUB_TOKEN`, `OPENAI_API_KEY`,
/// `AWS_SECRET_ACCESS_KEY`, `LOOM_NATIVE_AUTH_FILE` and anything else shaped
/// like a secret without enumerating providers — an enumeration goes stale the
/// first time someone adds a provider.
const CREDENTIAL_SHAPED: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "cookie",
    "auth",
    "session",
    "apikey",
];

/// Whether `name` is shaped like a credential and must never reach the child.
#[must_use]
pub fn credential_shaped(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_SHAPED
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Validate a caller-supplied readiness argv against [`READINESS_ALLOWLIST`].
///
/// # Errors
///
/// Returns an error when any token is outside the allowlist. The message names
/// the allowlist, never the rejected token: a rejected token is untrusted input
/// and may be a pasted secret.
pub fn validate_readiness(tokens: &[String]) -> Result<Vec<String>> {
    if tokens.is_empty() {
        return Ok(DEFAULT_READINESS.iter().map(|&t| t.to_owned()).collect());
    }
    anyhow::ensure!(
        tokens
            .iter()
            .all(|t| READINESS_ALLOWLIST.contains(&t.as_str())),
        "readiness argv contains a token outside the provider-free allowlist ({}); \
         refusing to run it rather than risk a model call",
        READINESS_ALLOWLIST.join(" ")
    );
    Ok(tokens.to_vec())
}

/// One attempt's private state tree: an isolated `HOME`, a private XDG set, and
/// the guarded OpenCode config directory holding the bindings a real guarded
/// launch writes.
///
/// Every directory is 0700 and owned by the current user, validated by
/// `native_tools::provision::private_directory` — the same check the production
/// launch path applies to its own per-launch state, reused rather than
/// reimplemented.
pub struct IsolatedState {
    pub root: PathBuf,
    pub home: PathBuf,
    pub config_dir: PathBuf,
    xdg: Vec<(&'static str, PathBuf)>,
}

impl IsolatedState {
    /// Create a fresh private tree under `parent`.
    ///
    /// # Errors
    ///
    /// Propagates any filesystem failure, including a directory that cannot be
    /// made 0700-private.
    pub fn create(parent: &Path) -> Result<Self> {
        let root = parent.join(uuid::Uuid::new_v4().to_string());
        crate::native_tools::provision::private_directory(&root)?;
        let home = root.join("home");
        crate::native_tools::provision::private_directory(&home)?;
        let xdg: Vec<(&'static str, PathBuf)> = [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ]
        .into_iter()
        .map(|(key, leaf)| (key, root.join(leaf)))
        .collect();
        for (_, path) in &xdg {
            crate::native_tools::provision::private_directory(path)?;
        }
        let config_dir = root.join("opencode-config");
        crate::native_tools::provision::private_directory(&config_dir)?;
        Ok(Self {
            root,
            home,
            config_dir,
            xdg,
        })
    }

    /// The directory the pinned plugin package set is materialized into.
    #[must_use]
    pub fn package_root(&self) -> &Path {
        &self.config_dir
    }
}

/// A bounded, provider-free probe of one installed harness binary.
pub struct Probe {
    bin: PathBuf,
    package_manager: PathBuf,
    deadline: Duration,
    network: NetworkMode,
    readiness: Vec<String>,
}

impl Probe {
    /// Build a probe. `readiness` is validated against the allowlist here, so a
    /// `Probe` value cannot hold an argv that could reach a model.
    ///
    /// # Errors
    ///
    /// Propagates [`validate_readiness`]'s refusal.
    pub fn new(
        bin: PathBuf,
        deadline: Duration,
        network: NetworkMode,
        readiness: &[String],
    ) -> Result<Self> {
        Ok(Self {
            bin,
            package_manager: PathBuf::from(DEFAULT_PACKAGE_MANAGER),
            deadline,
            network,
            readiness: validate_readiness(readiness)?,
        })
    }

    /// Override the package-manager binary used by [`Self::resolve_packages`].
    ///
    /// The report names whichever one ran, so a host that resolves the pin with
    /// something other than `npm` stays honestly described rather than
    /// silently measured under the wrong resolver.
    #[must_use]
    pub fn with_package_manager(mut self, bin: PathBuf) -> Self {
        self.package_manager = bin;
        self
    }

    /// The package-manager binary this probe will use.
    #[must_use]
    pub fn package_manager(&self) -> &Path {
        &self.package_manager
    }

    /// The validated provider-free argv this probe will use.
    #[must_use]
    pub fn readiness_argv(&self) -> &[String] {
        &self.readiness
    }

    /// Build a child command with a from-scratch environment.
    ///
    /// # Errors
    ///
    /// Returns an error if the assembled environment somehow contains a
    /// credential-shaped name — a belt-and-braces assertion over the allowlist
    /// above, not a recoverable condition.
    pub fn command(&self, state: &IsolatedState, argv: &[String]) -> Result<Command> {
        let mut command = Command::new(&self.bin);
        // Nothing the parent holds is admitted except the explicitly inherited
        // names below: an inherited ZAI_API_KEY/GH_TOKEN is precisely what this
        // probe must not be able to spend or leak.
        command.env_clear();
        command
            .current_dir(&state.root)
            .stdin(Stdio::null())
            .args(argv)
            .env("HOME", &state.home)
            .env("PWD", &state.root)
            .env("OPENCODE_CONFIG_DIR", &state.config_dir)
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .env("LOOM_NATIVE_READINESS_PROBE", "1")
            .env("CI", "1")
            .env("NO_COLOR", "1");
        for (key, path) in &state.xdg {
            command.env(key, path);
        }
        for key in INHERITED {
            if let Some(value) = std::env::var_os(key).filter(|v| !v.is_empty()) {
                command.env(key, value);
            }
        }
        if self.network == NetworkMode::DeniedByEnv {
            deny_network(&mut command);
        }
        assert_credential_free(&command)?;
        Ok(command)
    }

    /// Measure [`super::Stage::BinaryProbe`]: the binary answers `--version`.
    ///
    /// Returns the observation plus the sanitized version line when readable.
    #[must_use]
    pub fn version(&self, state: &IsolatedState) -> (Observation, Option<String>) {
        let argv = vec!["--version".to_owned()];
        let Ok(command) = self.command(state, &argv) else {
            return (local_failure(0), None);
        };
        let ran = run_stage(command, self.deadline);
        match &ran.observation {
            Observation::Measured { millis } => {
                let line = super::sanitize_version_line(&String::from_utf8_lossy(&ran.stdout));
                if line.is_none() {
                    return (
                        Observation::Failed {
                            classification: Classification::UnparsableVersion,
                            elapsed_millis: *millis,
                            stdout_bytes: ran.stdout.len(),
                            stderr_bytes: ran.stderr_bytes,
                        },
                        None,
                    );
                }
                (ran.observation, line)
            }
            _ => (ran.observation, None),
        }
    }

    /// Measure [`super::Stage::ServerSessionReady`]: the provider-free argv
    /// returns inside the deadline.
    #[must_use]
    pub fn readiness(&self, state: &IsolatedState) -> Observation {
        let Ok(command) = self.command(state, &self.readiness.clone()) else {
            return local_failure(0);
        };
        run_stage(command, self.deadline).observation
    }

    /// Measure [`super::Stage::PackageResolution`] by materializing the pinned
    /// plugin package set with `npm` into `state.package_root()`.
    ///
    /// This is **equivalent** package work, not necessarily the identical
    /// resolver OpenCode itself uses — the report says so. What matters for the
    /// question at hand is that it is the same pinned dependency, the same
    /// platform, and observably separable from every other boundary.
    #[must_use]
    pub fn resolve_packages(&self, state: &IsolatedState) -> Observation {
        let mut command = Command::new(&self.package_manager);
        command.env_clear();
        command
            .current_dir(state.package_root())
            .stdin(Stdio::null())
            .args([
                "install",
                "--omit=dev",
                "--no-audit",
                "--no-fund",
                "--loglevel=error",
            ])
            .env("HOME", &state.home)
            .env("npm_config_update_notifier", "false")
            .env("npm_config_fund", "false")
            .env("CI", "1")
            .env("NO_COLOR", "1");
        for (key, path) in &state.xdg {
            command.env(key, path);
        }
        for key in INHERITED {
            if let Some(value) = std::env::var_os(key).filter(|v| !v.is_empty()) {
                command.env(key, value);
            }
        }
        if self.network == NetworkMode::DeniedByEnv {
            deny_network(&mut command);
        }
        if assert_credential_free(&command).is_err() {
            return local_failure(0);
        }
        run_stage(command, self.deadline).observation
    }
}

/// Point every proxy/registry variable a well-behaved HTTP client honours at a
/// closed loopback port.
///
/// Port 1 is privileged and unbound, so a connection attempt is refused
/// immediately rather than hanging — a denial that costs no wall time.
fn deny_network(command: &mut Command) {
    const CLOSED: &str = "http://127.0.0.1:1";
    for key in [
        "http_proxy",
        "https_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env(key, CLOSED);
    }
    command
        .env("no_proxy", "")
        .env("NO_PROXY", "")
        .env("npm_config_proxy", CLOSED)
        .env("npm_config_https_proxy", CLOSED)
        .env("npm_config_registry", "http://127.0.0.1:1/")
        .env("npm_config_offline", "true")
        .env("npm_config_audit", "false");
}

/// Assert no credential-shaped variable survived into `command`'s environment.
fn assert_credential_free(command: &Command) -> Result<()> {
    let offending = command
        .get_envs()
        .filter_map(|(key, value)| value.map(|_| key))
        .find(|key| credential_shaped(&key.to_string_lossy()));
    anyhow::ensure!(
        offending.is_none(),
        // The NAME is safe to omit as well: a caller that hits this has a bug,
        // and the fix is in this module, not in the message.
        "refusing to launch a readiness probe: a credential-shaped variable \
         reached the child environment"
    );
    Ok(())
}

/// A local (pre-child) failure, reported as a boundary failure with no bytes.
fn local_failure(elapsed_millis: u64) -> Observation {
    Observation::Failed {
        classification: Classification::LocalSetupFailed,
        elapsed_millis,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

/// What one bounded child run produced.
struct Ran {
    observation: Observation,
    stdout: Vec<u8>,
    stderr_bytes: usize,
}

/// Run `command` under `deadline` and convert the outcome into an
/// [`Observation`].
///
/// The child's bytes are retained only for the caller that needs the version
/// line; every failure path reports counts and a [`Classification`], never
/// content.
fn run_stage(command: Command, deadline: Duration) -> Ran {
    let started = Instant::now();
    let completion = run_bounded(command, deadline);
    let elapsed_millis = elapsed_millis(started);
    match completion {
        Ok(Completion::Exited(output)) => {
            let classification = if output.status.success() {
                None
            } else if output.status.code().is_none() {
                Some(Classification::SignalDeath)
            } else {
                Some(Classification::NonzeroExit)
            };
            let observation = match classification {
                None => Observation::Measured {
                    millis: elapsed_millis,
                },
                Some(classification) => Observation::Failed {
                    classification,
                    elapsed_millis,
                    stdout_bytes: output.stdout.len(),
                    stderr_bytes: output.stderr.len(),
                },
            };
            Ran {
                observation,
                stderr_bytes: output.stderr.len(),
                stdout: output.stdout,
            }
        }
        Ok(Completion::TimedOut { stdout, stderr }) => Ran {
            observation: Observation::Failed {
                classification: Classification::Timeout,
                elapsed_millis,
                stdout_bytes: stdout.len(),
                stderr_bytes: stderr.len(),
            },
            stderr_bytes: stderr.len(),
            stdout: Vec::new(),
        },
        Err(error) => Ran {
            observation: Observation::Failed {
                classification: match error {
                    ExecError::Spawn(_) => Classification::SpawnFailed,
                    ExecError::Collect(_) => Classification::ExitUnobserved,
                },
                elapsed_millis,
                stdout_bytes: 0,
                stderr_bytes: 0,
            },
            stderr_bytes: 0,
            stdout: Vec::new(),
        },
    }
}

/// Wall milliseconds since `started`, saturating rather than wrapping.
fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Write the guarded bindings into an isolated state tree, measuring
/// [`super::Stage::BindingProvision`].
///
/// Delegates to `native_tools::provision::write_opencode_bindings`, the same
/// writer the production guarded launch path uses, so a measured provisioning
/// cost can never drift from the bytes production actually writes.
#[must_use]
pub fn provision_bindings(state: &IsolatedState) -> Observation {
    let started = Instant::now();
    match crate::native_tools::provision::write_opencode_bindings(&state.config_dir) {
        Ok(()) => Observation::Measured {
            millis: elapsed_millis(started),
        },
        Err(_) => local_failure(elapsed_millis(started)),
    }
}

/// Read the pinned plugin manifest bytes a guarded launch provisions, for the
/// cache identity's integrity component.
///
/// # Errors
///
/// Propagates a read failure of the provisioned manifest.
pub fn manifest_bytes(state: &IsolatedState) -> Result<Vec<u8>> {
    std::fs::read(state.config_dir.join("package.json"))
        .context("cannot read the provisioned plugin manifest")
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;
