//! A Loom-owned `CARGO_TARGET_DIR` for every role run (issue #8370, part a).
//!
//! # The leak this closes
//!
//! Before this module, a worker spawn only got a `CARGO_TARGET_DIR` from Loom
//! when the repo opted in to the per-worktree scheme (#8458) **and** the spawn
//! owned a sweep's claim. Every other run (a role-runner builder/doctor/judge
//! tick, an in-session sweep) got nothing, and the agent improvised one:
//! `<repo>/.loom/target-doctor-<N>`, `/tmp/loom-target-*`,
//! `~/.cache/cargo-target-*`, `/tmp/cargo-target-review-*`. Nothing owned
//! those, so nothing removed them; one host carried 85 GB of them.
//!
//! # The scheme
//!
//! * One named root, `<repo_root>/.loom/targets/`, gitignored by the managed
//!   `.gitignore` block. A run's dir is `<root>/.loom/targets/<role>-<run-id>`.
//! * [`crate::role_runner`] plans the path before it spawns a tick and passes it
//!   to the child as [`RUN_TARGET_DIR_ENV`]. It holds a [`RunDirGuard`] for the
//!   life of the tick and removes the dir when the child is gone, on every
//!   outcome. That is the run-end half: `worker_spawn` ends in `exec()`, so
//!   nothing inside the spawn chain survives to clean up after the agent.
//! * [`crate::worker_spawn`] (the seam every dispatch surface converges on)
//!   decides with [`decide`], creates the dir with [`provision`] (which also
//!   records the harness pid in [`OWNER_FILE`], since `exec()` keeps the pid),
//!   and exports it. A spawn with no planned path (a daemon sweep, a manual
//!   `spawn-worker.sh`) derives one; nothing waits on those, so
//!   [`crate::target_orphan_reclaim`] collects them once the owner is gone.
//!
//! # Precedence (first match wins)
//!
//! 1. The #8458 per-worktree dir, when `spawn_target_dir` returned one.
//! 2. An operator-set `CARGO_TARGET_DIR` already in the environment.
//! 3. A containerized re-entry (`LOOM_SPAWN_CONTAINERIZED`): the outer spawn
//!    already passed an explicit `-e CARGO_TARGET_DIR`.
//! 4. A repo with no `Cargo.toml` at its root: there is nothing to build.
//!
//! Each of those exports nothing new. Otherwise the run gets its own dir.

use std::path::{Path, PathBuf};

/// The child-environment variable carrying the path `role_runner` planned for
/// this run, so the spawn creates exactly the directory the runner will remove.
pub const RUN_TARGET_DIR_ENV: &str = "LOOM_RUN_TARGET_DIR";

/// Directory name under `<repo_root>/.loom/` that holds every run's dir.
pub const TARGETS_DIR_NAME: &str = "targets";

/// File inside a run dir recording the pid of the harness that builds into it.
/// `worker_spawn` writes it just before `exec()`, which keeps the pid, so it
/// names the agent process for the whole run.
pub const OWNER_FILE: &str = ".loom-run-owner";

/// `<repo_root>/.loom/targets`.
#[must_use]
pub fn targets_root(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join(TARGETS_DIR_NAME)
}

/// Keep `[A-Za-z0-9_-]` and map everything else to `-`, so a role or run id
/// can never add a path component or a `..`.
fn sanitize(segment: &str) -> String {
    let cleaned: String = segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "run".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The dir for one run: `<repo_root>/.loom/targets/<role>-<run_id>`.
#[must_use]
pub fn planned_for(repo_root: &Path, role: &str, run_id: &str) -> PathBuf {
    targets_root(repo_root).join(format!("{}-{}", sanitize(role), sanitize(run_id)))
}

/// Whether `path` has the shape of a run dir: absolute, named
/// `[A-Za-z0-9_-]+`, directly inside a `targets` dir directly inside `.loom`.
///
/// Root-independent on purpose: the spawn and the runner may spell the repo
/// root differently (a symlinked checkout, `/private/tmp` vs `/tmp`), and this
/// is the check that stands between an environment variable and a
/// `remove_dir_all`.
#[must_use]
pub fn is_run_target_dir(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name().and_then(|n| n.to_str()) != Some(TARGETS_DIR_NAME) {
        return false;
    }
    parent
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        == Some(".loom")
}

/// Everything [`decide`] reads, passed in so the decision is testable without
/// mutating process-global environment.
#[derive(Debug, Clone, Default)]
pub struct SpawnInputs<'a> {
    /// What `spawn_target_dir` (#8458) returned for this spawn.
    pub per_worktree: Option<&'a Path>,
    /// `CARGO_TARGET_DIR` already in the spawn's environment.
    pub ambient: Option<&'a str>,
    /// `LOOM_SPAWN_CONTAINERIZED` is set.
    pub containerized: bool,
    /// [`RUN_TARGET_DIR_ENV`], when the launcher planned a path.
    pub planned: Option<&'a str>,
    /// The role name (`LOOM_ROLE`), used only to name a derived dir.
    pub role: Option<&'a str>,
    /// Unique suffix for a derived dir.
    pub run_id: &'a str,
}

/// The run dir this spawn should create and export, or `None` to leave the
/// environment alone. Pure: creates nothing (see [`provision`]).
#[must_use]
pub fn decide(repo_root: &Path, inputs: &SpawnInputs<'_>) -> Option<PathBuf> {
    if inputs.per_worktree.is_some() {
        return None;
    }
    if inputs.ambient.is_some_and(|v| !v.trim().is_empty()) {
        return None;
    }
    if inputs.containerized {
        return None;
    }
    if !repo_root.join("Cargo.toml").is_file() {
        return None;
    }
    if let Some(planned) = inputs
        .planned
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
    {
        if is_run_target_dir(&planned) {
            return Some(planned);
        }
        log::warn!(
            "run_target_dir: ignoring {RUN_TARGET_DIR_ENV}={} (not a <repo>/.loom/targets/<name> \
             path); deriving one instead",
            planned.display()
        );
    }
    Some(planned_for(repo_root, inputs.role.unwrap_or("worker"), inputs.run_id))
}

/// Create `dir` and record `owner_pid` in it. `None` when either step fails:
/// a `CARGO_TARGET_DIR` cargo cannot write to fails every build, so a failed
/// create exports nothing (the same rule as `spawn_target_dir`).
#[must_use]
pub fn provision(dir: &Path, owner_pid: u32) -> Option<PathBuf> {
    if !is_run_target_dir(dir) {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    std::fs::write(dir.join(OWNER_FILE), format!("{owner_pid}\n")).ok()?;
    Some(dir.to_path_buf())
}

/// The pid recorded in `dir`'s [`OWNER_FILE`], if readable.
#[must_use]
pub fn owner_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join(OWNER_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// What [`remove_run_dir`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    /// The dir was removed.
    Removed,
    /// Nothing was there (the spawn never created it: not a Cargo repo, an
    /// operator `CARGO_TARGET_DIR`, a containerized run).
    Absent,
    /// Kept: the recorded owner is still running, the path is not a run dir,
    /// or it is a symlink.
    Kept(String),
    /// The removal itself failed.
    Failed(String),
}

/// Remove one run dir at run end. Refuses anything that is not a run dir or is
/// a symlink, and keeps a dir whose recorded owner is still alive (a tick that
/// lost track of its child must not pull the target out from under it; the
/// orphan sweep collects it later).
pub fn remove_run_dir(
    dir: &Path,
    owner_alive: &dyn Fn(u32) -> bool,
    remove: &dyn Fn(&Path) -> std::io::Result<()>,
) -> Removal {
    if !is_run_target_dir(dir) {
        return Removal::Kept("not a .loom/targets run dir".to_string());
    }
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Removal::Absent,
        Err(e) => return Removal::Failed(e.to_string()),
    };
    if !meta.is_dir() {
        return Removal::Kept("not a directory (symlink or file)".to_string());
    }
    if let Some(pid) = owner_pid(dir).filter(|&p| owner_alive(p)) {
        return Removal::Kept(format!("owner pid {pid} is still running"));
    }
    match remove(dir) {
        Ok(()) => Removal::Removed,
        Err(e) => Removal::Failed(e.to_string()),
    }
}

/// Holds one run's planned dir for the life of a role tick and removes it on
/// drop, whatever the outcome. Removal is best-effort: a failure is logged and
/// never changes the tick's result.
#[derive(Debug)]
pub struct RunDirGuard {
    dir: PathBuf,
}

impl RunDirGuard {
    /// Plan `<repo_root>/.loom/targets/<role>-<run_id>`.
    #[must_use]
    pub fn plan(repo_root: &Path, role: &str, run_id: &str) -> Self {
        Self {
            dir: planned_for(repo_root, role, run_id),
        }
    }

    /// The planned path.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Pass the planned path to the child.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        cmd.env(RUN_TARGET_DIR_ENV, &self.dir);
    }

    /// Remove now, with the liveness probe and remover injected (tests).
    pub fn finish_with(
        &self,
        owner_alive: &dyn Fn(u32) -> bool,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> Removal {
        let outcome = remove_run_dir(&self.dir, owner_alive, remove);
        match &outcome {
            Removal::Removed => log::info!(
                "run_target_dir: removed run target dir {} at run end",
                self.dir.display()
            ),
            Removal::Absent => {}
            Removal::Kept(why) => log::info!(
                "run_target_dir: kept {} at run end ({why}); the orphan sweep will collect it",
                self.dir.display()
            ),
            Removal::Failed(why) => log::warn!(
                "run_target_dir: could not remove {} at run end: {why}",
                self.dir.display()
            ),
        }
        outcome
    }
}

impl Drop for RunDirGuard {
    fn drop(&mut self) {
        let _ = self
            .finish_with(&crate::live_claim::pid_is_live_process, &|p| std::fs::remove_dir_all(p));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
