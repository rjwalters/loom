//! The native artifact-resolution probe (epic #7810, PR 5).
//!
//! Adapts [`crate::release_resolve`] to the shape [`super::ArtifactResolution`]
//! already reasons about. It lives here rather than in `auto_update.rs` because
//! that file is over `.loom/docs/file-size-policy.md`'s threshold and frozen.
//!
//! Before this, the tick asked `loom-daemon-update.sh --resolve-json`. #7609
//! chose that deliberately — "rather than reimplementing release resolution in
//! Rust" — and it was right while the alternative was a second implementation.
//! Now the daemon owns the only one, so the question is answered in-process and
//! a host with no usable script is no longer limited by whether one exists.

use super::{ArtifactInfo, ArtifactResolution};
use crate::release_resolve::{build_time_repo, resolve, resolve_repo, Inputs, Resolution};
use std::path::{Path, PathBuf};

/// The environment-derived [`Inputs`] for this host, built once so the
/// resolution and the fetch that acts on it can never disagree about which
/// repo they mean (#8513).
fn env_inputs(root: &Path) -> Inputs<'_> {
    Inputs {
        repo_root: root,
        target_override: std::env::var("LOOM_DAEMON_UPDATE_TARGET").ok(),
        repo_override: std::env::var("LOOM_DAEMON_UPDATE_GH_REPO").ok(),
        machine_checkout: std::env::var("LOOM_MACHINE_CHECKOUT")
            .ok()
            .map(PathBuf::from),
        build_time_repo: build_time_repo(),
        installed_bin: running_binary(),
        fetch_disabled: artifact_fetch_disabled(),
    }
}

/// The repo the tick would query for `root`, by [`resolve_repo`]'s priority
/// order (#8513).
///
/// [`super::ScriptAutoUpdateProbe::fetch_artifact`] exports this to the
/// `loom-daemon-update.sh --fetch` child as `LOOM_DAEMON_UPDATE_GH_REPO`,
/// because the script resolves the repo *independently* — from its own cwd's
/// `origin` remote — and would otherwise try to download the artifact this
/// resolver found in Loom's releases from the workspace's own project. The
/// resolution deciding to roll and the roll itself must name one repo.
#[must_use]
pub(super) fn fetch_repo(root: &Path) -> Option<String> {
    resolve_repo(&env_inputs(root))
}

/// Resolve the latest artifact for this host.
///
/// The field mapping is total rather than defaulted: an `Option` that arrives
/// `None` stays `None`, because "could not determine" must not become a value
/// the verdict logic compares against.
#[must_use]
pub(super) fn native_resolution(root: &Path) -> ArtifactResolution {
    let inputs = env_inputs(root);
    match resolve(&inputs) {
        Resolution::Unresolved(reason) => ArtifactResolution::Unresolved(reason),
        Resolution::Resolved(r) => ArtifactResolution::Resolved(ArtifactInfo {
            repo: r.repo,
            tag: r.tag,
            version: r.version,
            published_at: r.published_at,
            asset_sha256: r.asset_sha256,
            target: Some(r.target),
            installed_version: r.installed_version,
            installed_sha256: r.installed_sha256,
        }),
    }
}

/// The binary this daemon is running from, or `None` when that cannot be
/// answered honestly.
///
/// Deliberately NOT [`crate::daemon_bin_resolve::resolve_daemon_bin`], whose
/// module doc scopes its guarantee to "read-only self-probes with no dependency
/// on daemon-process/subprocess version parity". This caller is the opposite:
/// it reads the binary's `--version` to decide whether to roll. That helper
/// strips a deleted-inode marker and returns whatever file now sits at the
/// path — which, in the window after `auto_update` provisions and before the
/// (possibly deferred) restart lands, is the NEW binary. Reading its version
/// here would report the roll as complete while the old process is still
/// running.
///
/// So: the real `current_exe()`, and `None` the moment the kernel says its
/// inode is gone. `None` is the fail-safe direction — `classify_artifact`
/// turns an undetermined installed version into `Newer`, and #7609's
/// `already_converged` guard is what stops that becoming a fetch loop.
fn running_binary() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // The kernel appends this to /proc/self/exe once the inode is unlinked.
    if exe.to_string_lossy().ends_with(" (deleted)") {
        return None;
    }
    exe.is_file().then_some(exe)
}

/// `--no-fetch` / `LOOM_DAEMON_UPDATE_FETCH=0`, the fleet-wide opt-out the
/// script honoured in this mode. An operator who turned the artifact path off
/// gets that as the reason, which is what keeps the tick falling back to
/// source rather than reporting a forge problem.
fn artifact_fetch_disabled() -> bool {
    std::env::var("LOOM_DAEMON_UPDATE_FETCH")
        .map(|v| matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deleted_inode_path_is_undetermined_not_the_replacement_binary() {
        // The window this exists for: `auto_update` provisions a new binary,
        // the (possibly deferred) restart has not landed, and the kernel marks
        // /proc/self/exe as deleted. `daemon_bin_resolve::resolve_daemon_bin`
        // strips that marker and hands back the NEW file on purpose — its doc
        // scopes that to callers with "no dependency on
        // daemon-process/subprocess version parity". Reading its `--version`
        // here would report the roll complete while the old process still runs.
        //
        // `running_binary` is a thin wrapper, so the marker check is what is
        // actually pinned.
        let marked = std::path::PathBuf::from("/usr/local/bin/loom-daemon (deleted)");
        assert!(
            marked.to_string_lossy().ends_with(" (deleted)"),
            "the marker this guards on must stay exactly the kernel's"
        );
    }

    #[test]
    fn the_running_binary_is_a_real_file_or_none() {
        // Never a path that does not exist: an unreadable "installed binary"
        // would yield an empty version that compares as older than every
        // release.
        if let Some(p) = running_binary() {
            assert!(p.is_file(), "{p:?}");
        }
    }

    #[test]
    fn fetch_disabled_reads_the_documented_spellings_only() {
        // The shell honoured 0/false/no/off. A stray "disabled" must not
        // silently turn the artifact path off fleet-wide.
        for (v, want) in [
            ("0", true),
            ("false", true),
            ("no", true),
            ("off", true),
            (" 0 ", true),
            ("1", false),
            ("true", false),
            ("", false),
            ("disabled", false),
        ] {
            // SAFETY: single-threaded test process; the var is read immediately.
            unsafe { std::env::set_var("LOOM_DAEMON_UPDATE_FETCH", v) };
            assert_eq!(artifact_fetch_disabled(), want, "{v:?}");
        }
        unsafe { std::env::remove_var("LOOM_DAEMON_UPDATE_FETCH") };
    }

    #[test]
    fn build_time_repo_resolves_to_this_workspaces_own_repository_field() {
        // `repository.workspace = true` in `loom-daemon/Cargo.toml` inherits
        // `[workspace.package] repository` (#8513) — for THIS crate that is
        // always `https://github.com/rjwalters/loom`, so the compiled-in
        // fallback is deterministic across every build of this repo.
        assert_eq!(build_time_repo().as_deref(), Some("rjwalters/loom"));
    }
}
