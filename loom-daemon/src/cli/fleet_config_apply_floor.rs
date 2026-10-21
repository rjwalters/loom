//! `loom-daemon fleet-config apply-floor` — bring the installed CLI binary up
//! to the fleet floor on a host that has no daemon (Issue #12926).
//!
//! The floor roll (#10698, [`crate::auto_update::floor_roll`]) runs
//! inside a running daemon, so an attended host declared
//! `loom-daemon: expected: absent` has no actor that converges its binary.
//! This verb is that actor, as a composition of the existing pieces: store
//! sync → compare with [`floor::parse_triple`] → release resolve → verified
//! fetch → [`provision::stage`] / [`provision::publish`] (one `.previous`
//! backup kept).
//!
//! It only replaces the file on disk. It never starts, restarts or signals a
//! daemon.
//!
//! The floor is read from a snapshot the forge confirmed current in this
//! invocation ([`Policy::FailClosed`]): a daemon-less host has no fleet-sync
//! pass keeping `fleet-sync-status.json` fresh, and acting on a stale floor
//! could install the wrong release. Forge unreachable ⇒ non-zero, nothing
//! touched.
//!
//! Exit codes: `0` installed, or nothing to do; `2` the floor could not be
//! applied (store/forge/fetch/verify/install failure, unsatisfiable floor).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;

use crate::auto_update::floor_roll::{floor_verdict, FloorVerdict, Release};
use crate::daemon_update::{artifact, provision};
use crate::fleet_store::fetch::{self, Policy, Snapshot};
use crate::fleet_store::floor::{self, FloorRead};
use crate::fleet_store::gh::GhTransport;
use crate::fleet_store::{self as store};
use crate::fleet_sync::FloorKnowledge;

/// The version of the binary that is running, i.e. the installed CLI.
pub(crate) const INSTALLED_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the floor says about the installed binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The store carries no `loom_min_version`.
    NoFloor,
    /// The store's floor cannot be read; never treated as "no floor".
    Malformed(String),
    /// Floor or installed version is not plain `X.Y.Z`.
    Uncomparable { floor: String },
    /// Installed is at or above the floor.
    AtOrAbove { floor: String },
    /// Installed is below the floor.
    Below { floor: String },
}

/// Classify `installed` against what the store says (pure).
pub(crate) fn decide(read: &FloorRead, installed: &str) -> Decision {
    match read {
        FloorRead::Absent => Decision::NoFloor,
        FloorRead::Malformed { source, detail } => {
            Decision::Malformed(format!("{source}: {detail}"))
        }
        FloorRead::Valid { version, .. } => {
            match (floor::parse_triple(version), floor::parse_triple(installed)) {
                (Some(min), Some(run)) if run >= min => Decision::AtOrAbove {
                    floor: version.clone(),
                },
                (Some(_), Some(_)) => Decision::Below {
                    floor: version.clone(),
                },
                _ => Decision::Uncomparable {
                    floor: version.clone(),
                },
            }
        }
    }
}

/// The `render --check` drift line, or `None` when the CLI is not below the
/// floor. Read-only; needs no network beyond the snapshot already loaded.
pub(crate) fn cli_below_floor_line(snapshot: &Snapshot, installed: &str) -> Option<String> {
    match decide(&floor::read(snapshot), installed) {
        Decision::Below { floor } => Some(format!(
            "CLI below floor: installed {installed}, floor {floor} (`loom-daemon fleet-config \
             apply-floor` replaces the binary; no daemon is started)"
        )),
        _ => None,
    }
}

/// Where releases and verified artifacts come from. Production is
/// [`GhSource`]; tests inject a fake.
pub(crate) trait Source {
    /// The newest release carrying this platform's binary.
    fn newest(&self) -> Result<Release>;
    /// Download `release`'s binary and verify it (SHA-256 unconditionally,
    /// signature when published). Returns the verified file.
    fn fetch(&self, release: &Release) -> Result<PathBuf>;
}

struct GhSource {
    repo_root: PathBuf,
}

impl Source for GhSource {
    fn newest(&self) -> Result<Release> {
        let r = artifact::fetch_resolve_latest(&self.repo_root, None);
        if !r.ok {
            return Err(anyhow!("could not resolve the newest release: {}", r.reason));
        }
        Ok(Release {
            tag: r.latest_tag,
            version: r.latest_version,
        })
    }

    fn fetch(&self, release: &Release) -> Result<PathBuf> {
        // Pin to the release the verdict compared, as the roll does.
        let r = artifact::fetch_resolve_latest(&self.repo_root, Some(&release.tag));
        if !r.ok {
            return Err(anyhow!("could not resolve release {}: {}", release.tag, r.reason));
        }
        let fetched = artifact::fetch_and_verify_artifact(
            &self.repo_root,
            &r.target,
            &r.repo_slug,
            &r.latest_tag,
        )
        .map_err(|e| match e {
            artifact::FetchFailure::Verification => anyhow!(
                "verification of release {} FAILED (checksum or signature); not installing",
                release.tag
            ),
            artifact::FetchFailure::Download => {
                anyhow!("could not download release {}", release.tag)
            }
        })?;
        Ok(fetched.bin)
    }
}

/// Decide and, when below the floor, install. Returns the exit code.
pub(crate) fn apply(
    read: &FloorRead,
    installed: &str,
    dest: &Path,
    dry_run: bool,
    source: &dyn Source,
) -> i32 {
    let floor = match decide(read, installed) {
        Decision::NoFloor => {
            println!("no `{}` in the fleet store: nothing to apply", floor::KEY);
            return 0;
        }
        Decision::Malformed(why) => {
            eprintln!("REFUSED: the fleet floor cannot be read ({why}); binary untouched");
            return 2;
        }
        Decision::Uncomparable { floor } => {
            eprintln!(
                "REFUSED: floor {floor} and installed {installed} are not both plain X.Y.Z; \
                 binary untouched"
            );
            return 2;
        }
        Decision::AtOrAbove { floor } => {
            println!("installed {installed} is at or above the fleet floor {floor}: nothing to do");
            return 0;
        }
        Decision::Below { floor } => floor,
    };
    println!("installed {installed} is below the fleet floor {floor}");

    let newest = match source.newest() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e:#}; binary untouched");
            return 2;
        }
    };
    let target = match floor_verdict(&FloorKnowledge::Set(floor.clone()), installed, Some(&newest))
    {
        FloorVerdict::Below { target, .. } => target,
        FloorVerdict::Unsatisfiable(report) => {
            eprintln!("{}", report.note());
            return 2;
        }
        other => {
            eprintln!(
                "REFUSED: release {} cannot be compared with floor {floor} ({other:?}); binary \
                 untouched",
                newest.version
            );
            return 2;
        }
    };
    if dry_run {
        println!(
            "--dry-run: would install {} ({}) over {}",
            target.tag,
            target.version,
            dest.display()
        );
        return 0;
    }

    let outcome = source.fetch(&target).and_then(|bin| {
        let staged = provision::stage(&bin, dest)
            .with_context(|| format!("staging beside {}", dest.display()))?;
        provision::publish(&staged, dest).map_err(|e| anyhow!("{e}"))
    });
    match outcome {
        Ok(done) => {
            println!("installed {} ({}) at {}", target.tag, target.version, dest.display());
            if let Some(line) = done.retained_line() {
                println!("{line}");
            }
            0
        }
        Err(e) => {
            eprintln!("install failed: {e:#}; the existing binary is untouched");
            2
        }
    }
}

/// Entry point for the CLI verb. Diverges so the verified-artifact scratch
/// dir is cleaned up on every path.
pub(crate) fn run(workspace: &Path, dry_run: bool) -> ! {
    let code = run_inner(workspace, dry_run).unwrap_or_else(|e| {
        eprintln!("loom-daemon fleet-config: error: {e:#}");
        2
    });
    crate::daemon_update::exit(code)
}

fn run_inner(workspace: &Path, dry_run: bool) -> Result<i32> {
    let effective = crate::config_resolver::resolve_effective_config(workspace);
    let Some(location) = store::resolve_location(&effective, &|k| std::env::var(k).ok())? else {
        println!("no fleet store configured for {}: nothing to apply", workspace.display());
        return Ok(0);
    };
    let cache = store::default_cache_dir(&location)?;
    let transport = GhTransport::new(workspace, &location.repo);
    // Confirmed current in this invocation, or an error (see module doc).
    let loaded = fetch::load(&transport, &cache, &location, Policy::FailClosed, Utc::now())?;
    let read = floor::read(&loaded.snapshot);
    let source = GhSource {
        repo_root: workspace.to_path_buf(),
    };
    Ok(apply(
        &read,
        INSTALLED_VERSION,
        &provision::provision_target(),
        dry_run,
        &source,
    ))
}

#[cfg(test)]
#[path = "fleet_config_apply_floor_tests.rs"]
mod tests;
