//! The mid-build watchdog's "did this sweep make build progress?" probe
//! (Issue #3895), split out of `watchdog.rs` so its failure arm can carry the
//! explanation Issue #8170 needed.

use std::path::Path;
use std::process::Command;

/// Whether `wt` exists AND has uncommitted changes — the "made build progress
/// then died" signal behind [`SweepRegistry::worktree_dirty`].
///
/// Runs `git -C <wt> status --porcelain --untracked-files=all`; any non-blank
/// output means dirty. Deliberately a plain `Command::new("git")` with no
/// `GIT_CONFIG_*` overrides: in production this runs against a real worktree
/// in a real checkout, where the operator's own git configuration is
/// *supposed* to apply. (Loom's own test fixture therefore neutralises the
/// host config repo-locally instead — see `FIXTURE_GIT_LOCAL_CONFIG` in
/// `sweep_registry::test_support`.)
///
/// # Why the failure arm logs (Issue #8170)
///
/// An unprobeable worktree resolves to `false`, which is the SAFE direction:
/// [`midbuild_decision`](super::midbuild_decision) then returns `Healthy` and
/// nothing destructive runs. But it is also completely silent, and a failed
/// probe is indistinguishable from a genuinely clean worktree to every
/// caller — so a `git` that cannot run turns the whole mid-build watchdog
/// (recovery *and* every refuse-to-destroy accounting path: `midbuild_inuse`,
/// `midbuild_gaveup`, `midbuild_lease_superseded`) into a no-op with no trace
/// at all. That silence is what made #8170's failure shape read as a fault in
/// the guards rather than in their input.
///
/// Not latched: the `!wt.exists()` early return above already absorbs the
/// common "no worktree yet" case, so reaching the warn means `git status`
/// itself failed on a directory that does exist — rare, and worth a line each
/// time it happens.
///
/// [`SweepRegistry::worktree_dirty`]: super::SweepRegistry::worktree_dirty
pub(crate) fn worktree_is_dirty(wt: &Path, issue: u32) -> bool {
    if !wt.exists() {
        return false;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(wt)
        .arg("status")
        .arg("--porcelain")
        .arg("--untracked-files=all")
        .output();
    match output {
        Ok(o) if o.status.success() => !o.stdout.iter().all(u8::is_ascii_whitespace),
        other => {
            let detail = match &other {
                Ok(o) => format!(
                    "git status exited {} ({})",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => format!("could not run git: {e}"),
            };
            log::warn!(
                "midbuild-watchdog: could not probe whether issue #{issue}'s worktree at {} is \
                 dirty — {detail}. Treating it as NOT dirty, so no recovery (and no destructive \
                 `git reset --hard`) will run for this issue until the probe works again.",
                wt.display(),
            );
            false
        }
    }
}
