//! The read-only half of `daemon-update` that runs BEFORE the checkout sync
//! (Issue #11064).
//!
//! `sync_with_origin` fetches and may fast-forward the source checkout (and
//! run `resync-installed.sh`). The fleet floor check (#11044) used to run
//! after it, so a refused run said "Nothing was changed" about a checkout it
//! had already moved. Everything the refusal depends on is read here first,
//! without writing anything:
//!
//! - the installed daemon ([`Installed::probe`]): which binary, its commit,
//!   and whether its version is absent, unreadable or known;
//! - `--to-floor` against that version ([`to_floor`]), which now refuses a
//!   binary whose version cannot be read instead of overwriting it;
//! - for a manual, unconfirmed run on a fleet host, the target and the floor
//!   check itself ([`floor_before_sync`]). The release resolution it makes is
//!   handed on, so the run installs exactly the release that was confirmed.
//!
//! A source build's target is the checkout's VERSION as it is now. When the
//! sync then moves the checkout, the post-sync check compares the new target
//! and says exactly what changed if it refuses ([`floor_guard::nothing_changed`]).

use std::path::{Path, PathBuf};

use super::args::{Args, FetchMode};
use super::artifact::{self, Resolution};
use super::floor_guard::{self, Decision, InstalledVersion, ToFloor};
use super::supervisor::Detected;
use super::{exit, out, util};
use crate::fleet_sync::offline_floor::offline_floor;
use crate::fleet_sync::FloorKnowledge;

/// The installed daemon, as the staleness comparison and `--to-floor` see it.
pub struct Installed {
    /// The PATH-resolved binary (the stale-entry-point advisory's input).
    pub daemon_bin: Option<PathBuf>,
    /// The binary the supervisor launches, else the PATH-resolved one (#6009).
    pub staleness_bin: Option<PathBuf>,
    /// Where `staleness_bin` came from, for the log.
    pub staleness_source: String,
    /// Its embedded short commit, or `unknown`.
    pub commit: String,
    /// Its version.
    pub version: InstalledVersion,
}

impl Installed {
    /// Resolve and run `--version` on the installed binary. Read-only.
    pub fn probe(repo_root: &Path, sup: &Detected) -> Installed {
        let daemon_bin = crate::daemon_start::paths::locate_daemon_bin(repo_root);
        // Compared against the binary the detected supervisor actually
        // launches, falling back to the PATH-resolved one only when no
        // supervisor is detected or its exec path could not be determined
        // (#6009) — PATH resolution alone cannot notice a supervisor pointed
        // at a different absolute path than the PATH-resolved one.
        let (staleness_bin, staleness_source) = match sup.supervisor_bin.as_ref() {
            Some(b) => (Some(b.clone()), format!("{} supervisor config", sup.manager.word())),
            None => (daemon_bin.clone(), "PATH resolution".to_string()),
        };
        let mut commit = "unknown".to_string();
        let mut version = InstalledVersion::Absent;
        if let Some(bin) = staleness_bin.as_ref().filter(|b| util::is_executable(b)) {
            let output = util::version_output(bin);
            let extracted = util::extract_commit(&output);
            if !extracted.is_empty() {
                commit = extracted;
            }
            let v = util::extract_version(&output);
            version = if v.is_empty() {
                InstalledVersion::Unreadable(bin.display().to_string())
            } else {
                InstalledVersion::Known(v)
            };
        }
        Installed {
            daemon_bin,
            staleness_bin,
            staleness_source,
            commit,
            version,
        }
    }
}

/// `--to-floor` against the installed version: exits 0 when there is
/// nothing to install, 1 when installing could move the host down, and
/// returns when the floor's release should be installed.
pub fn to_floor(a: &Args, installed: &Installed) {
    if !a.to_floor {
        return;
    }
    let tag = a.tag.as_deref().unwrap_or_default();
    match floor_guard::to_floor_decision(tag, &installed.version, a.force) {
        ToFloor::Install => {}
        ToFloor::Met(message) => {
            out::ok(&message);
            exit(0);
        }
        ToFloor::Refuse(message) => {
            out::err(&message);
            exit(1);
        }
    }
}

/// Whether the resolved release `r` is installed instead of a source build.
///
/// Strictly newer wins. An EQUAL version only wins under an explicit
/// `--fetch`: `--force` alone keeps its established meaning ("rebuild this
/// checkout even though it isn't stale"), which an operator running it
/// inside a source tree would be surprised to see silently turn into a
/// download.
#[must_use]
pub fn artifact_wins(r: &Resolution, installed_version: &str, fetch_mode: FetchMode) -> bool {
    let installed = if installed_version.is_empty() {
        "0.0.0"
    } else {
        installed_version
    };
    let cmp = util::semver_compare(&r.latest_version, installed);
    r.ok && (cmp.is_gt() || (cmp.is_eq() && fetch_mode == FetchMode::Force))
}

/// What [`floor_before_sync`] leaves for the rest of the run.
#[derive(Default)]
pub struct BeforeSync {
    /// The release resolution it made, reused so the run installs the
    /// release that was checked (and `gh` is asked once).
    pub resolution: Option<Resolution>,
    /// The version the operator saw the warning for and continued with: the
    /// post-sync check skips only when the real target is this exact version.
    pub confirmed: Option<String>,
}

/// The fleet floor check, before the checkout sync, for the runs it can
/// refuse: not read-only (`--check`, `--resolve-json`, `--dry-run`,
/// `--prune-stale-entry-points`), not confirmed (`--yes`,
/// `LOOM_DAEMON_UPDATE_YES`, `LOOM_DAEMON_UPDATE_INVOKER`), on a fleet host.
/// Every other run is left to the post-sync check, which cannot refuse them.
pub fn floor_before_sync(a: &Args, repo_root: &Path, installed: &Installed) -> BeforeSync {
    let mut done = BeforeSync::default();
    if a.check_only || a.resolve_json || a.dry_run || a.prune_stale {
        return done;
    }
    let invoker = util::env_non_empty(floor_guard::INVOKER_ENV);
    let yes = a.yes || util::env_truthy(floor_guard::YES_ENV);
    if floor_guard::decide(false, yes, invoker.as_deref(), false) == Decision::Proceed {
        return done;
    }
    if matches!(offline_floor(repo_root).knowledge, FloorKnowledge::NoStore) {
        return done;
    }
    let installed_version = installed.version.as_str();
    let check = |version: &str, how: &str| {
        floor_guard::enforce(
            repo_root,
            floor_guard::Target { version, how },
            installed_version,
            (false, false),
            None,
        )
    };

    if a.fetch_mode != FetchMode::Off {
        let r = artifact::fetch_resolve_latest(repo_root, a.tag.as_deref());
        let release = artifact_wins(&r, installed_version, a.fetch_mode)
            .then(|| (r.latest_version.clone(), format!("release {}", r.latest_tag)));
        let refused_later = (!r.ok && a.tag.is_some()) || a.fetch_mode == FetchMode::Force;
        done.resolution = Some(r);
        if let Some((version, how)) = release {
            done.confirmed = check(&version, &how).then_some(version);
            return done;
        }
        // No release to install: a pinned tag or `--fetch` refuses after
        // the sync, which then never fetches or fast-forwards (#7609).
        if refused_later {
            return done;
        }
    }

    // A source build of the checkout as it is now. A binary already built
    // from this HEAD has nothing to build until the sync moves it.
    let head = util::git(repo_root, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    if !a.force && !head.is_empty() && head == installed.commit {
        return done;
    }
    let version = std::fs::read_to_string(repo_root.join("VERSION"))
        .map(|t| t.chars().filter(|c| !c.is_whitespace()).collect::<String>())
        .unwrap_or_default();
    done.confirmed = check(&version, &format!("a source build of {head}")).then_some(version);
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(version: &str, ok: bool) -> Resolution {
        Resolution {
            target: "x86_64-unknown-linux-gnu".to_string(),
            repo_slug: "o/r".to_string(),
            latest_tag: format!("v{version}"),
            latest_version: version.to_string(),
            ok,
            reason: String::new(),
        }
    }

    #[test]
    fn a_release_wins_only_when_newer_or_equal_under_an_explicit_fetch() {
        let r = resolved("0.19.960", true);
        assert!(artifact_wins(&r, "0.19.950", FetchMode::Auto));
        assert!(!artifact_wins(&r, "0.19.960", FetchMode::Auto), "equal: no download");
        assert!(artifact_wins(&r, "0.19.960", FetchMode::Force), "equal under --fetch");
        assert!(!artifact_wins(&r, "0.19.970", FetchMode::Force), "never older");
        assert!(artifact_wins(&r, "", FetchMode::Auto), "no installed version: 0.0.0");
        assert!(!artifact_wins(&resolved("0.19.960", false), "0.19.950", FetchMode::Force));
    }
}
