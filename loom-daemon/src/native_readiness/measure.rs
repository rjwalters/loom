//! `loom-daemon worker readiness` — the cold/warm measurement loop and its
//! clap surface.
//!
//! # What an operator gets
//!
//! One JSON document per run: per-attempt boundary timings, per-boundary
//! min/median/max, the host's load average on both sides of each phase, and an
//! explicit ledger of the boundaries nothing here can observe. No model is
//! called, no credential is read, no forge is contacted, and nothing in the
//! output is a claim about provider latency or about an improvement.
//!
//! # Cold vs warm
//!
//! Both phases give every attempt a brand-new private state tree, so the
//! comparison isolates exactly one variable: whether the pinned package set is
//! resolved from scratch ([`super::Mode::Cold`]) or restored from the keyed
//! user-home cache ([`super::Mode::Warm`]). That is deliberate — it is the only
//! boundary whose work is shareable (see [`super::package_cache`]), so it is the
//! only one the two phases are allowed to differ on.
//!
//! Attempts run cold-phase-first and the report records each phase's own load
//! average, because phase order and host contention are both confounders and
//! the honest move is to disclose them rather than to correct for them.
//!
//! # `--deny-network`
//!
//! Turns the run into the controlled fixture the investigation needs: with the
//! network denied, a cold phase's package resolution is expected to *fail*
//! while the binary probe and binding provisioning still succeed, and a warm
//! phase with a populated cache is expected to pass all four. That contrast is
//! the evidence for which boundaries need network — far stronger than timing
//! alone, and it costs nothing.

use super::{
    package_cache::{CacheIdentity, PackageCache},
    probe::{self, IsolatedState, Probe},
    AttemptReport, CacheOutcome, Classification, HostConditions, Mode, NetworkMode, Observation,
    PhaseReport, ReadinessReport, Stage, StageObservation,
};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The relative paths a warm attempt shares through the cache. Package
/// artifacts only — see [`super::package_cache`] for the enforcement.
///
/// Left at `node_modules` after the #8600 live run, which confirmed this is
/// exactly the tree OpenCode's own resolver produces (see
/// [`super::probe::DEFAULT_PACKAGE_MANAGER`]) and therefore the right thing to
/// share. That run also found that publishing the **real** dependency tree is
/// refused by [`super::package_cache`]'s own safety rules — `node_modules/.bin`
/// holds symlinks, and `kubernetes-types/storage` trips
/// [`super::package_cache::FORBIDDEN_COMPONENTS`] — so a warm phase currently
/// degrades to [`CacheOutcome::Bypassed`] against the pinned CLI. That is
/// reported honestly per attempt rather than hidden, and is tracked separately;
/// narrowing this list would not fix it, because both blockers are inside the
/// one artifact a warm launch needs.
const SHARED_ARTIFACTS: &[&str] = &["node_modules"];

/// Which phases to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Phases {
    /// Fresh package resolution every attempt.
    Cold,
    /// Package artifacts restored from the keyed cache.
    Warm,
    /// Cold first, then warm, in one report.
    Both,
}

#[derive(clap::Args)]
pub struct ReadinessArgs {
    /// Harness binary to probe. Defaults to `LOOM_OPENCODE_BIN`, then
    /// `opencode`. Point it at the CLI itself, not at a wrapper script that
    /// derives its install prefix from `$HOME`: every attempt runs with an
    /// isolated `HOME`, so such a wrapper resolves to a path that does not
    /// exist and exits 127 before the CLI ever runs (#8600).
    #[arg(long, value_name = "PATH")]
    bin: Option<PathBuf>,
    /// Attempts per phase.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=20))]
    attempts: u32,
    /// Which phases to measure.
    #[arg(long, value_enum, default_value_t = Phases::Both)]
    phases: Phases,
    /// Hard wall deadline for every individual boundary. Not a cost cap —
    /// nothing here costs money.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
    deadline_seconds: u64,
    /// Deny the child network access via proxy/registry environment variables.
    #[arg(long)]
    deny_network: bool,
    /// One provider-free readiness argv token; repeatable. Refused unless every
    /// token is on the provider-free allowlist.
    #[arg(long = "readiness-arg", value_name = "TOKEN")]
    readiness: Vec<String>,
    /// Shared package-artifact cache base. Must be under the user home and
    /// outside every repository. Defaults to
    /// `~/.local/state/loom/native-packages`.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
    /// Package manager used to materialize the pinned plugin set.
    #[arg(long, default_value = probe::DEFAULT_PACKAGE_MANAGER, value_name = "BIN")]
    package_manager: PathBuf,
    /// Warm the shared cache and report only that startup cost. Measures no
    /// phases; makes no model call.
    #[arg(long)]
    preflight: bool,
}

impl ReadinessArgs {
    /// Run the measurement (or the preflight warm) and print one JSON document.
    ///
    /// # Errors
    ///
    /// Fails on a refused readiness argv, an unusable scratch/cache directory,
    /// or a harness binary that cannot be executed at all. The report is
    /// printed before any such failure is raised, so a diagnostic run never
    /// loses the evidence it gathered.
    pub fn run(self) -> Result<()> {
        let bin = self.bin.clone().unwrap_or_else(|| {
            std::env::var_os("LOOM_OPENCODE_BIN")
                .filter(|v| !v.is_empty())
                .map_or_else(|| PathBuf::from("opencode"), PathBuf::from)
        });
        let network = if self.deny_network {
            NetworkMode::DeniedByEnv
        } else {
            NetworkMode::Allowed
        };
        let probe =
            Probe::new(bin, Duration::from_secs(self.deadline_seconds), network, &self.readiness)?
                .with_package_manager(self.package_manager.clone());

        let scratch = tempfile::Builder::new()
            .prefix("loom-readiness-")
            .tempdir()
            .context("cannot create a private scratch directory")?;
        crate::native_tools::provision::outside_every_repository(scratch.path())?;

        let cache = self.open_cache()?;
        if self.preflight {
            return self.preflight(&probe, scratch.path(), cache.as_ref());
        }

        let modes: &[Mode] = match self.phases {
            Phases::Cold => &[Mode::Cold],
            Phases::Warm => &[Mode::Warm],
            Phases::Both => &[Mode::Cold, Mode::Warm],
        };
        let mut phases = Vec::new();
        let mut version = None;
        for &mode in modes {
            let mut host = HostConditions::begin();
            let mut attempts = Vec::new();
            for index in 0..self.attempts {
                let (attempt, seen) =
                    self.attempt(index as usize, mode, &probe, scratch.path(), cache.as_ref());
                version = version.or(seen);
                attempts.push(attempt);
            }
            host.finish();
            phases.push(PhaseReport::new(mode, host, attempts));
        }

        let report = ReadinessReport::new(
            network,
            probe.readiness_argv().to_vec(),
            version,
            cache.as_ref().map(|c| c.base().display().to_string()),
            phases,
        );
        println!("{}", serde_json::to_string(&report)?);
        anyhow::ensure!(
            !unspawnable(&report),
            "the harness binary could not be executed; install it or point --bin/LOOM_OPENCODE_BIN at one"
        );
        Ok(())
    }

    /// Open the shared cache, unless no phase will consult it.
    fn open_cache(&self) -> Result<Option<PackageCache>> {
        if matches!(self.phases, Phases::Cold) && !self.preflight {
            return Ok(None);
        }
        let cache = match &self.cache_dir {
            Some(dir) => PackageCache::open(dir)?,
            None => PackageCache::user_home()?,
        };
        Ok(Some(cache))
    }

    /// Bounded preflight warming: populate the cache once and report that cost
    /// on its own.
    ///
    /// The payload names its own attribution (`startup_only`) and states that
    /// no model executed, so a later efficiency report cannot fold this number
    /// into a per-token cost without contradicting the document it came from.
    fn preflight(&self, probe: &Probe, scratch: &Path, cache: Option<&PackageCache>) -> Result<()> {
        let started = Instant::now();
        let (attempt, version) = self.attempt(0, Mode::Warm, probe, scratch, cache);
        let payload = serde_json::json!({
            "schema": 1,
            "phase": "preflight_warming",
            "model_calls": 0,
            "model_execution": "none",
            "paid_retry": false,
            "forge_contact": false,
            "cost_attribution": "startup_only",
            "reported_separately_from_model_execution": true,
            "wall_millis": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "cli_version": version,
            "plugin_load_observed": attempt.plugin_load_observed,
            "cache": attempt.cache,
            "cache_base": cache.map(|c| c.base().display().to_string()),
            "stages": attempt.stages,
            "unknown_boundaries": Stage::ALL
                .iter()
                .filter_map(|s| s.unknown_reason().map(|r| serde_json::json!({"stage": s, "reason": r})))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string(&payload)?);
        Ok(())
    }

    /// One traversal of the boundary set.
    ///
    /// Returns the attempt plus the CLI version line if this attempt read one.
    /// A failed boundary stops the traversal: every later measurable boundary
    /// is [`Observation::NotReached`], never zero.
    fn attempt(
        &self,
        index: usize,
        mode: Mode,
        probe: &Probe,
        scratch: &Path,
        cache: Option<&PackageCache>,
    ) -> (AttemptReport, Option<String>) {
        let started = Instant::now();
        let mut stages = Vec::new();
        let mut outcome = if mode == Mode::Cold || cache.is_none() {
            CacheOutcome::Bypassed
        } else {
            CacheOutcome::Miss
        };
        let mut version = None;

        let state = match IsolatedState::create(scratch) {
            Ok(state) => state,
            Err(_) => {
                return (finish(index, mode, outcome, started, setup_failed()), None);
            }
        };

        let (observation, seen) = probe.version(&state);
        let mut ok = matches!(observation, Observation::Measured { .. });
        version = version.or(seen);
        stages.push(StageObservation {
            stage: Stage::BinaryProbe,
            observation,
        });

        let observation = if ok {
            let observation = probe::provision_bindings(&state);
            ok = matches!(observation, Observation::Measured { .. });
            observation
        } else {
            Observation::NotReached
        };
        stages.push(StageObservation {
            stage: Stage::BindingProvision,
            observation,
        });

        let observation = if ok {
            let (observation, seen) = self.packages(probe, &state, mode, cache, version.as_deref());
            outcome = seen;
            ok = matches!(observation, Observation::Measured { .. });
            observation
        } else {
            Observation::NotReached
        };
        stages.push(StageObservation {
            stage: Stage::PackageResolution,
            observation,
        });

        let (observation, plugin_load_observed) = if ok {
            probe.readiness(&state)
        } else {
            (Observation::NotReached, None)
        };
        stages.push(StageObservation {
            stage: Stage::ServerSessionReady,
            observation,
        });

        stages.extend(unknown_boundaries());
        (
            AttemptReport {
                index,
                mode,
                cache: outcome,
                total_millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                plugin_load_observed,
                stages,
            },
            version,
        )
    }

    /// Measure [`Stage::PackageResolution`], using the cache in warm mode.
    ///
    /// A warm hit measures the *restore*, which is the cost a warm launch would
    /// actually pay — not zero, and not the install it avoided.
    fn packages(
        &self,
        probe: &Probe,
        state: &IsolatedState,
        mode: Mode,
        cache: Option<&PackageCache>,
        version: Option<&str>,
    ) -> (Observation, CacheOutcome) {
        let (Some(cache), Some(version), Mode::Warm) = (cache, version, mode) else {
            // Cold, cacheless, or no readable version to key on: resolve from
            // scratch and share nothing. An unkeyable run must never reuse an
            // entry keyed on a version it could not confirm.
            return (probe.resolve_packages(state), CacheOutcome::Bypassed);
        };
        let Ok(manifest) = probe::manifest_bytes(state) else {
            return (setup_failed_observation(), CacheOutcome::Bypassed);
        };
        let identity = CacheIdentity::new(version, &plugin_pin(&manifest), &manifest);
        let started = Instant::now();
        match cache.lookup(&identity) {
            Ok((Some(entry), outcome)) => {
                let restored = cache.restore(&entry, state.package_root());
                let observation = if restored.is_ok() {
                    Observation::Measured {
                        millis: millis(started),
                    }
                } else {
                    setup_failed_observation()
                };
                (observation, outcome)
            }
            Ok((None, outcome)) => {
                let resolved = probe.resolve_packages(state);
                if !matches!(resolved, Observation::Measured { .. }) {
                    return (resolved, outcome);
                }
                match cache.publish(&identity, state.package_root(), SHARED_ARTIFACTS) {
                    Ok(_) => (
                        Observation::Measured {
                            millis: millis(started),
                        },
                        outcome,
                    ),
                    // The install succeeded; only sharing it failed. Report the
                    // measured resolution and say the cache was not populated.
                    Err(_) => (resolved, CacheOutcome::Bypassed),
                }
            }
            Err(_) => (probe.resolve_packages(state), CacheOutcome::Bypassed),
        }
    }
}

/// Derive the pinned dependency spec from the provisioned manifest bytes.
///
/// Read from the manifest rather than hardcoded so the cache key tracks the
/// pin a launch actually writes. An unparsable manifest yields `"unknown"`,
/// which still keys distinctly because the manifest digest is a separate
/// identity component.
#[must_use]
pub fn plugin_pin(manifest: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(manifest) else {
        return "unknown".to_owned();
    };
    let Some(dependencies) = value.get("dependencies").and_then(|d| d.as_object()) else {
        return "unknown".to_owned();
    };
    let mut pins: Vec<String> = dependencies
        .iter()
        .filter_map(|(name, spec)| spec.as_str().map(|spec| format!("{name}@{spec}")))
        .collect();
    if pins.is_empty() {
        return "unknown".to_owned();
    }
    pins.sort();
    pins.join(",")
}

/// The three boundaries a provider-free probe cannot observe, with reasons.
fn unknown_boundaries() -> Vec<StageObservation> {
    Stage::ALL
        .iter()
        .filter_map(|&stage| {
            stage.unknown_reason().map(|reason| StageObservation {
                stage,
                observation: Observation::Unknown { reason },
            })
        })
        .collect()
}

fn setup_failed_observation() -> Observation {
    Observation::Failed {
        classification: Classification::LocalSetupFailed,
        elapsed_millis: 0,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

/// Every boundary marked unreachable because the scratch tree failed.
fn setup_failed() -> Vec<StageObservation> {
    let mut stages: Vec<StageObservation> = Stage::ALL
        .iter()
        .filter(|s| s.provider_free_observable())
        .map(|&stage| StageObservation {
            stage,
            observation: if stage == Stage::BinaryProbe {
                setup_failed_observation()
            } else {
                Observation::NotReached
            },
        })
        .collect();
    stages.extend(unknown_boundaries());
    stages
}

fn finish(
    index: usize,
    mode: Mode,
    cache: CacheOutcome,
    started: Instant,
    stages: Vec<StageObservation>,
) -> AttemptReport {
    AttemptReport {
        index,
        mode,
        cache,
        total_millis: millis(started),
        // The scratch tree failed before any child ran, so no readiness
        // invocation existed to observe a plugin load from.
        plugin_load_observed: None,
        stages,
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Whether every attempt failed to even start the harness binary.
///
/// Distinguished from an ordinary boundary failure because it means the run
/// measured nothing at all: an operator pointed the probe at a binary that is
/// not installed, which is a configuration error rather than a finding.
#[must_use]
pub fn unspawnable(report: &ReadinessReport) -> bool {
    let attempts: Vec<&AttemptReport> = report
        .phases
        .iter()
        .flat_map(|phase| phase.attempts.iter())
        .collect();
    !attempts.is_empty()
        && attempts.iter().all(|attempt| {
            matches!(
                attempt.stage(Stage::BinaryProbe),
                Some(Observation::Failed {
                    classification: Classification::SpawnFailed,
                    ..
                })
            )
        })
}

#[cfg(test)]
#[path = "measure_tests.rs"]
mod tests;
