//! Reclaim a worktree's **redirected** Cargo target directory when the
//! worktree is removed (issue #7239).
//!
//! # The leak
//!
//! Cargo's build output is not always `<workspace>/target`. `CARGO_TARGET_DIR`
//! or `build.target-dir` in any `config.toml` on the lookup path redirects it
//! anywhere — commonly onto a large external volume, one directory per
//! worktree. Every worktree-removal path in Loom (this crate's
//! [`super::clean::cleanup_worktree`], driven by `loom-daemon clean` and by the
//! periodic [`crate::worktree_reaper`]; and `worktree.sh remove` on the bash
//! side) only ever removed the worktree directory itself, so a redirected
//! target dir outlived its worktree forever. One multi-agent host accumulated
//! tens of orphaned directories and hundreds of GB before the volume started
//! pressuring other workloads.
//!
//! # What this module adds
//!
//! Two steps, deliberately split, because they must happen on opposite sides
//! of the removal:
//!
//! 1. [`resolve_for_worktree`] — run **before** the worktree is removed:
//!    `cargo metadata` needs the worktree's manifest, which is gone the instant
//!    `git worktree remove` runs.
//! 2. [`reclaim`] — run **after** the worktree is off disk, so the worktree
//!    being removed can never count as a live referent of its own target dir.
//!
//! # Never-delete gates
//!
//! Deleting a build cache is cheap to redo and catastrophic to get wrong, so
//! [`plan_reclaim`] refuses unless *all* of these hold:
//!
//! - The resolved path is **outside** the worktree (an in-worktree `target/`
//!   already disappears with the worktree — nothing to do).
//! - It is not a path this pass must never touch: the repository itself or any
//!   ancestor of it, the primary checkout's own `target/` (that belongs to
//!   [`crate::deep_clean`], which gates on disk pressure and the machine build
//!   slot), `$HOME`, or a suspiciously shallow path.
//! - **No other live worktree resolves to it.** The `host-optimize` convention
//!   is a *single shared* `target-dir` for the whole machine; deleting that on
//!   one worktree's removal would destroy a sibling's cache mid-build.
//!   Containment counts as sharing in both directions, and an *unanswerable*
//!   sharing question (git could not list the worktrees at all) refuses too —
//!   "nobody else uses it" and "we could not find out" must not look alike.
//! - **No running process** is using it — the same evidence-based gate
//!   [`super::clean::sweep_primary_checkout_artifacts`] applies to the primary
//!   checkout's artifacts (issue #6127).
//!
//! # Relationship to the bash implementation
//!
//! `defaults/scripts/lib/cargo-target-dir.sh` implements the same rules for
//! `worktree.sh remove`. The two are parallel implementations on purpose: the
//! daemon reaps worktrees in repositories where Loom's bash library may not be
//! installed at all, so shelling out to it would silently no-op exactly where
//! the leak was observed. Both mirror the resolution order of the standalone
//! `scripts/cargo-target-dir.sh`, and both have a parity test against it
//! ([`tests::resolution_matches_the_standalone_bash_resolver`] here;
//! `defaults/scripts/tests/test-cargo-target-dir-reclaim.sh` there) so a change
//! to one that is not made to the others fails CI.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Best-effort physical path for comparison purposes. A path that does not
/// exist is returned unchanged rather than dropped — containment checks still
/// need something to compare.
fn realish(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Resolve a possibly-relative Cargo path against a workspace root, without
/// requiring it to exist (the target dir is created by the build itself).
fn absolutize(value: &str, workspace_root: &Path) -> PathBuf {
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    }
}

/// Cargo's resolution order for a workspace's target directory, with both
/// external inputs injected so it is unit-testable without mutating this
/// process's environment (`set_var` is process-global and racy under a
/// multi-threaded test runner):
///
/// 1. `CARGO_TARGET_DIR` when set and non-empty — env beats config in Cargo.
/// 2. `cargo metadata`'s `target_directory`, which applies the full
///    `config.toml` hierarchy (including `build.target-dir`).
/// 3. `<workspace_root>/target` — Cargo's default, i.e. the assumption every
///    caller made before this existed.
///
/// Mirrors `scripts/cargo-target-dir.sh` step for step; see the module docs.
#[must_use]
pub fn resolve_target_dir_with(
    workspace_root: &Path,
    env_override: Option<&str>,
    metadata: &dyn Fn(&Path) -> Option<String>,
) -> PathBuf {
    if let Some(value) = env_override {
        if !value.is_empty() {
            return absolutize(value, workspace_root);
        }
    }
    if let Some(resolved) = metadata(workspace_root) {
        if !resolved.is_empty() && resolved != "null" {
            return absolutize(&resolved, workspace_root);
        }
    }
    workspace_root.join("target")
}

/// `cargo metadata --format-version 1 --no-deps`'s `target_directory`, or
/// `None` when cargo is unavailable / the invocation failed / the field is
/// missing. Parsed with a targeted string extraction (cargo emits compact
/// single-line JSON), matching the `sed` fallback in the bash twin — no serde
/// round-trip of a large document just to read one field.
#[must_use]
pub fn cargo_metadata_target_directory(workspace_root: &Path) -> Option<String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let key = "\"target_directory\":\"";
    let start = stdout.find(key)? + key.len();
    let rest = &stdout[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Files Cargo would read `build.target-dir` from, for `workspace_root`:
/// `.cargo/config.toml` (and the legacy extension-less `config`) in every
/// ancestor directory, then `$CARGO_HOME`.
fn cargo_config_candidates(workspace_root: &Path, cargo_home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir = Some(workspace_root);
    while let Some(current) = dir {
        out.push(current.join(".cargo").join("config.toml"));
        out.push(current.join(".cargo").join("config"));
        dir = current.parent();
    }
    if let Some(home) = cargo_home {
        out.push(home.join("config.toml"));
        out.push(home.join("config"));
    }
    out
}

/// Cheap pre-check: is a redirect even conceivable for this workspace?
///
/// When it returns `false` the target dir is provably `<workspace_root>/target`
/// and the caller can skip the `cargo metadata` subprocess entirely — which
/// keeps an ordinary (unredirected) host at a few small file reads per
/// worktree removal instead of a cargo invocation per removal.
#[must_use]
pub fn redirect_possible_with(
    workspace_root: &Path,
    env_override: Option<&str>,
    cargo_home: Option<&Path>,
) -> bool {
    if env_override.is_some_and(|v| !v.is_empty()) {
        return true;
    }
    // No manifest ⇒ cargo never built here ⇒ nothing to redirect.
    if !workspace_root.join("Cargo.toml").is_file() {
        return false;
    }
    cargo_config_candidates(workspace_root, cargo_home)
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .any(|body| {
            body.lines()
                .any(|line| line.trim_start().starts_with("target-dir"))
        })
}

fn env_cargo_target_dir() -> Option<String> {
    std::env::var("CARGO_TARGET_DIR")
        .ok()
        .filter(|v| !v.is_empty())
}

fn cargo_home() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CARGO_HOME") {
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".cargo"))
}

/// Production resolution for one worktree. **Must be called while the worktree
/// still exists on disk** — `cargo metadata` needs its manifest.
#[must_use]
pub fn resolve_for_worktree(worktree_path: &Path) -> PathBuf {
    let env = env_cargo_target_dir();
    if !redirect_possible_with(worktree_path, env.as_deref(), cargo_home().as_deref()) {
        return worktree_path.join("target");
    }
    resolve_target_dir_with(worktree_path, env.as_deref(), &cargo_metadata_target_directory)
}

/// What [`plan_reclaim`] decided about one worktree's resolved target dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetDirOutcome {
    /// Not redirected: it lives inside the worktree and is removed with it.
    Inside(PathBuf),
    /// Nothing on disk at the resolved path (never built, or already gone).
    Absent(PathBuf),
    /// A path this pass must never delete. `reason` says which rule fired.
    Refused { path: PathBuf, reason: String },
    /// Another live worktree resolves to the same directory.
    Shared { path: PathBuf, by: PathBuf },
    /// Live process(es) are using it.
    Protected { path: PathBuf, holders: Vec<String> },
    /// Dry run: this is what a real pass would remove.
    WouldReclaim { path: PathBuf, size_human: String },
    /// Removed.
    Reclaimed { path: PathBuf, size_human: String },
    /// Removal was attempted and failed.
    Failed { path: PathBuf, error: String },
}

impl TargetDirOutcome {
    /// One operator-facing line, or `None` for the two uninteresting outcomes
    /// that describe every unredirected host (`Inside` / `Absent`). Rendered
    /// identically by the interactive `clean` pass and the unattended reaper's
    /// log, so "why did disk not get freed?" has the same answer in both.
    #[must_use]
    pub fn report_line(&self) -> Option<String> {
        match self {
            Self::Inside(_) | Self::Absent(_) => None,
            Self::Refused { path, reason } => {
                Some(format!("Refusing to reclaim cargo target dir {} — {reason}", path.display()))
            }
            Self::Shared { path, by } => Some(format!(
                "Keeping redirected cargo target dir {} — still used by {}",
                path.display(),
                by.display()
            )),
            Self::Protected { path, holders } => Some(format!(
                "Keeping redirected cargo target dir {} — {} live process(es) [{}] still using \
                 it; the reclaim is deferred, not lost",
                path.display(),
                holders.len(),
                holders.join(", ")
            )),
            Self::WouldReclaim { path, size_human } => Some(format!(
                "Would reclaim redirected cargo target dir: {} ({size_human})",
                path.display()
            )),
            Self::Reclaimed { path, size_human } => Some(format!(
                "Reclaimed redirected cargo target dir: {} ({size_human})",
                path.display()
            )),
            Self::Failed { path, error } => Some(format!(
                "Could not reclaim redirected cargo target dir {} — {error}",
                path.display()
            )),
        }
    }
}

/// Every external input [`plan_reclaim`] consults, injected so the whole
/// decision — including the destructive step — is unit-testable without a real
/// process table, a real `git worktree list`, or a real `rm -rf`.
pub struct TargetDirProbes<'a> {
    /// Worktrees (and the primary checkout) git currently knows about, or
    /// `None` when that could not be determined at all (git missing, not a
    /// repo, I/O error). `None` is **not** the same as "no other worktrees":
    /// it is the one input whose absence makes the sharing gate unanswerable,
    /// so [`plan_reclaim`] refuses rather than deleting on an empty answer.
    pub live_worktrees: &'a dyn Fn() -> Option<Vec<PathBuf>>,
    /// True when a path is a workspace cargo actually builds in. A tree with
    /// no manifest cannot depend on a target dir, and counting it would make
    /// an ambient absolute `CARGO_TARGET_DIR` — which resolves identically for
    /// every path — report every redirected dir as shared, reclaiming nothing.
    pub has_manifest: &'a dyn Fn(&Path) -> bool,
    /// One live worktree's resolved target dir.
    pub resolve: &'a dyn Fn(&Path) -> PathBuf,
    /// Descriptions of live processes using a directory (empty ⇒ nobody).
    pub holders: &'a dyn Fn(&Path) -> Vec<String>,
    /// Human-readable directory size, for the report.
    pub size_human: &'a dyn Fn(&Path) -> String,
    /// Recursive removal.
    pub remove: &'a dyn Fn(&Path) -> std::io::Result<()>,
}

/// Decide (and, unless `dry_run`, act on) one worktree's resolved target dir.
///
/// `worktree_path` is the worktree that was removed; it is excluded from the
/// sharing scan by path, so this is correct whether it is still on disk (dry
/// run) or already gone (the real pass).
#[must_use]
pub fn plan_reclaim(
    repo_root: &Path,
    worktree_path: &Path,
    resolved: &Path,
    dry_run: bool,
    probes: &TargetDirProbes,
) -> TargetDirOutcome {
    let resolved_real = realish(resolved);
    let worktree_real = realish(worktree_path);
    let repo_real = realish(repo_root);

    // 1. The default, in-worktree location: it goes away with the worktree.
    if resolved_real == worktree_real || resolved_real.starts_with(&worktree_real) {
        return TargetDirOutcome::Inside(resolved.to_path_buf());
    }

    // 2. Paths that must never be deleted by this pass, however they resolved.
    let refuse = |reason: &str| TargetDirOutcome::Refused {
        path: resolved.to_path_buf(),
        reason: reason.to_string(),
    };
    if resolved_real.components().count() < 3 {
        // `/`, `/tmp`, `C:\` — one component for the root prefix plus at most
        // one name. Nothing Loom provisions is ever that shallow.
        return refuse("suspiciously shallow path");
    }
    if repo_real.starts_with(&resolved_real) {
        return refuse("contains the repository itself");
    }
    if resolved_real == repo_real.join("target") {
        // Regenerable, but it belongs to the pressure-gated deep-clean pass,
        // never to a single worktree's removal.
        return refuse("the primary checkout's own target/");
    }
    if std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .is_some_and(|home| realish(Path::new(&home)) == resolved_real)
    {
        return refuse("resolves to $HOME");
    }

    // 3. Nothing there.
    if !resolved_real.is_dir() {
        return TargetDirOutcome::Absent(resolved.to_path_buf());
    }

    // 4. Shared with a still-live worktree. Containment counts in BOTH
    //    directions: a sibling building into a parent of this path, or into a
    //    subtree of it, is equally destroyed by an `rm -rf` here.
    //
    //    Fail CLOSED when the worktree list is unavailable: an empty answer
    //    and "git could not tell us" are indistinguishable downstream, and
    //    treating the latter as "nobody else uses it" is precisely how a
    //    sibling's cache gets deleted mid-build. Mirrors the same rule
    //    `registered_worktree_paths`' own doc states for orphan removal.
    let Some(others) = (probes.live_worktrees)() else {
        return refuse("could not enumerate live worktrees (git worktree list failed)");
    };
    for other in others {
        let other_real = realish(&other);
        if other_real == worktree_real {
            continue;
        }
        if !(probes.has_manifest)(&other_real) {
            continue;
        }
        let other_target = realish(&(probes.resolve)(&other_real));
        if other_target == resolved_real
            || resolved_real.starts_with(&other_target)
            || other_target.starts_with(&resolved_real)
        {
            return TargetDirOutcome::Shared {
                path: resolved.to_path_buf(),
                by: other,
            };
        }
    }

    // 5. A running process is using it. Checked under `dry_run` too: a preview
    //    that claims it "would remove" a live build's output is a preview an
    //    operator would act on.
    let holders = (probes.holders)(&resolved_real);
    if !holders.is_empty() {
        return TargetDirOutcome::Protected {
            path: resolved.to_path_buf(),
            holders,
        };
    }

    let size_human = (probes.size_human)(&resolved_real);
    if dry_run {
        return TargetDirOutcome::WouldReclaim {
            path: resolved.to_path_buf(),
            size_human,
        };
    }
    match (probes.remove)(&resolved_real) {
        Ok(()) => TargetDirOutcome::Reclaimed {
            path: resolved.to_path_buf(),
            size_human,
        },
        Err(e) => TargetDirOutcome::Failed {
            path: resolved.to_path_buf(),
            error: e.to_string(),
        },
    }
}

/// Production wiring for [`plan_reclaim`]: real `git worktree list`, real
/// resolution, the real process table, real `du`-equivalent sizing, and a real
/// `remove_dir_all`.
#[must_use]
pub fn reclaim(
    repo_root: &Path,
    worktree_path: &Path,
    resolved: &Path,
    dry_run: bool,
) -> TargetDirOutcome {
    let live_worktrees = || {
        super::clean::registered_worktree_paths(repo_root)
            .map(|set| set.into_iter().collect::<Vec<_>>())
    };
    let has_manifest = |p: &Path| p.join("Cargo.toml").is_file();
    let resolve = |p: &Path| resolve_for_worktree(p);
    let holders = |p: &Path| {
        let mut out: Vec<String> = super::safety::find_processes_executing_within(p)
            .into_iter()
            .map(|exe| exe.to_string())
            .collect();
        for pid in super::safety::find_processes_using_directory(p) {
            let line = format!("pid {pid}");
            if !out.iter().any(|held| held.starts_with(&line)) {
                out.push(line);
            }
        }
        out
    };
    let size_human = |p: &Path| super::clean::dir_size_human(p);
    let remove = |p: &Path| std::fs::remove_dir_all(p);
    let probes = TargetDirProbes {
        live_worktrees: &live_worktrees,
        has_manifest: &has_manifest,
        resolve: &resolve,
        holders: &holders,
        size_human: &size_human,
        remove: &remove,
    };
    plan_reclaim(repo_root, worktree_path, resolved, dry_run, &probes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A repo root + one worktree + an external target dir, all real paths so
    /// the `is_dir` / canonicalize gates behave exactly as in production.
    struct Fixture {
        _tmp: tempfile::TempDir,
        repo_root: PathBuf,
        worktree: PathBuf,
        external: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let repo_root = base.join("repo");
        let worktree = repo_root.join(".loom/worktrees/issue-7239");
        let external = base.join("cargo-target/issue-7239");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(external.join("debug")).unwrap();
        std::fs::write(external.join("debug/artifact.bin"), vec![0u8; 2048]).unwrap();
        Fixture {
            _tmp: tmp,
            repo_root,
            worktree,
            external,
        }
    }

    /// A worktree that really does build with cargo (the production
    /// `has_manifest` probe, verbatim).
    fn make_cargo_worktree(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    }

    fn has_manifest(p: &Path) -> bool {
        p.join("Cargo.toml").is_file()
    }

    fn size_stub(_: &Path) -> String {
        "12.3G".to_string()
    }

    #[test]
    fn reclaims_an_external_target_dir_no_other_worktree_references() {
        let f = fixture();
        // The primary checkout has no manifest here, so it is correctly not a
        // referent — the shape the daemon sees on a repo it merely hosts.
        let live = vec![f.repo_root.clone(), f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes);

        assert_eq!(
            outcome,
            TargetDirOutcome::Reclaimed {
                path: f.external.clone(),
                size_human: "12.3G".to_string()
            }
        );
        assert!(!f.external.exists(), "the external target dir was not removed");
        assert!(outcome.report_line().unwrap().contains("Reclaimed"));
    }

    #[test]
    fn never_removes_a_target_dir_a_live_worktree_still_resolves_to() {
        let f = fixture();
        let sibling = f.repo_root.join(".loom/worktrees/issue-999");
        make_cargo_worktree(&sibling);
        let live = vec![f.worktree.clone(), sibling.clone()];
        let live_worktrees = || Some(live.clone());
        // The host-optimize shape: every worktree resolves to ONE shared dir.
        let shared = f.external.clone();
        let resolve = move |_: &Path| shared.clone();
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes);

        assert_eq!(
            outcome,
            TargetDirOutcome::Shared {
                path: f.external.clone(),
                by: sibling
            }
        );
        assert!(
            f.external.join("debug/artifact.bin").is_file(),
            "a shared target dir and its contents must survive intact"
        );
        assert!(removed.borrow().is_empty(), "nothing may be removed");
        assert!(outcome.report_line().unwrap().contains("still used by"));
    }

    #[test]
    fn a_sibling_building_into_a_parent_of_this_dir_also_counts_as_shared() {
        let f = fixture();
        let sibling = f.repo_root.join(".loom/worktrees/issue-999");
        make_cargo_worktree(&sibling);
        let live = vec![f.worktree.clone(), sibling];
        let live_worktrees = || Some(live.clone());
        let parent = f.external.parent().unwrap().to_path_buf();
        let resolve = move |_: &Path| parent.clone();
        let holders = |_: &Path| Vec::new();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes),
            TargetDirOutcome::Shared { .. }
        ));
        assert!(f.external.exists());
    }

    #[test]
    fn a_manifestless_live_worktree_is_not_treated_as_a_referent() {
        // Regression guard for the failure mode that would make this feature
        // reclaim nothing, ever: an ambient absolute CARGO_TARGET_DIR resolves
        // identically for EVERY path, so a tree that never builds with cargo
        // must not count as sharing the dir.
        let f = fixture();
        let bystander = f.repo_root.join(".loom/worktrees/issue-888");
        std::fs::create_dir_all(&bystander).unwrap(); // no Cargo.toml
        let live = vec![f.worktree.clone(), bystander];
        let live_worktrees = || Some(live.clone());
        let shared = f.external.clone();
        let resolve = move |_: &Path| shared.clone();
        let holders = |_: &Path| Vec::new();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes),
            TargetDirOutcome::Reclaimed { .. }
        ));
        assert!(!f.external.exists());
    }

    #[test]
    fn refuses_when_the_live_worktree_list_is_unavailable() {
        // Fail CLOSED: `git worktree list` failing is not evidence that
        // nothing else uses this directory. Treating an unanswerable sharing
        // question as "unshared" is exactly how a sibling's cache would get
        // deleted mid-build on a host with a broken/absent git.
        let f = fixture();
        let live_worktrees = || None;
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        for dry_run in [false, true] {
            match plan_reclaim(&f.repo_root, &f.worktree, &f.external, dry_run, &probes) {
                TargetDirOutcome::Refused { reason, .. } => {
                    assert!(reason.contains("enumerate"), "reason: {reason}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert!(f.external.join("debug/artifact.bin").is_file());
        assert!(removed.borrow().is_empty());
    }

    #[test]
    fn never_removes_a_target_dir_a_live_process_is_using() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| vec!["pid 4242 → /vol/cargo-target/debug/safehoused".to_string()];
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, false, &probes);

        assert!(matches!(outcome, TargetDirOutcome::Protected { .. }));
        assert!(f.external.exists(), "a dir backing a live process survives");
        assert!(removed.borrow().is_empty());
        assert!(outcome.report_line().unwrap().contains("4242"));
    }

    #[test]
    fn the_live_process_gate_applies_under_dry_run_too() {
        // A preview that claims it "would remove" a live build's output is a
        // preview an operator would act on.
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| vec!["pid 7 → /vol/cargo-target/debug/thing".to_string()];
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        assert!(matches!(
            plan_reclaim(&f.repo_root, &f.worktree, &f.external, true, &probes),
            TargetDirOutcome::Protected { .. }
        ));
    }

    #[test]
    fn dry_run_reports_a_size_and_removes_nothing() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &f.external, true, &probes);

        assert_eq!(
            outcome,
            TargetDirOutcome::WouldReclaim {
                path: f.external.clone(),
                size_human: "12.3G".to_string()
            }
        );
        assert!(f.external.exists(), "a dry run must not delete");
        assert!(removed.borrow().is_empty());
        let line = outcome.report_line().unwrap();
        assert!(line.contains("Would reclaim") && line.contains("12.3G"));
    }

    #[test]
    fn an_in_worktree_target_dir_is_a_silent_no_op() {
        let f = fixture();
        let inside = f.worktree.join("target");
        std::fs::create_dir_all(&inside).unwrap();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &inside, false, &probes);

        assert_eq!(outcome, TargetDirOutcome::Inside(inside.clone()));
        assert!(outcome.report_line().is_none(), "must not be chatty");
        assert!(removed.borrow().is_empty());
    }

    #[test]
    fn refuses_the_primary_checkouts_own_target_and_any_ancestor_of_the_repo() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| Vec::new();
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let remove = |p: &Path| {
            removed.borrow_mut().push(p.to_path_buf());
            std::fs::remove_dir_all(p)
        };
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let primary_target = f.repo_root.join("target");
        std::fs::create_dir_all(&primary_target).unwrap();
        match plan_reclaim(&f.repo_root, &f.worktree, &primary_target, false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("primary checkout"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(primary_target.exists());

        let ancestor = f.repo_root.parent().unwrap().to_path_buf();
        match plan_reclaim(&f.repo_root, &f.worktree, &ancestor, false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("repository itself"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(f.repo_root.exists(), "the repository must still be there");

        match plan_reclaim(&f.repo_root, &f.worktree, Path::new("/tmp"), false, &probes) {
            TargetDirOutcome::Refused { reason, .. } => {
                assert!(reason.contains("shallow"), "reason: {reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        assert!(removed.borrow().is_empty(), "no refusal may delete anything");
    }

    #[test]
    fn absent_target_dir_is_a_silent_no_op() {
        let f = fixture();
        let live = vec![f.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let resolve = |_: &Path| PathBuf::from("/nowhere/else");
        let holders = |_: &Path| Vec::new();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            holders: &holders,
            size_human: &size_stub,
            remove: &remove,
        };

        let never_built = f.external.parent().unwrap().join("issue-does-not-exist");
        let outcome = plan_reclaim(&f.repo_root, &f.worktree, &never_built, false, &probes);
        assert_eq!(outcome, TargetDirOutcome::Absent(never_built));
        assert!(outcome.report_line().is_none());
    }

    #[test]
    fn resolution_precedence_env_then_metadata_then_default() {
        let root = Path::new("/w/root");
        let no_metadata = |_: &Path| None;
        let metadata = |_: &Path| Some("/from/metadata".to_string());

        assert_eq!(
            resolve_target_dir_with(root, Some("/abs/target"), &no_metadata),
            PathBuf::from("/abs/target"),
            "env wins outright"
        );
        assert_eq!(
            resolve_target_dir_with(root, Some("rel-target"), &no_metadata),
            PathBuf::from("/w/root/rel-target"),
            "a relative env value resolves against the workspace root"
        );
        assert_eq!(
            resolve_target_dir_with(root, Some("/abs/target"), &metadata),
            PathBuf::from("/abs/target"),
            "env beats config, exactly as in Cargo"
        );
        assert_eq!(
            resolve_target_dir_with(root, None, &metadata),
            PathBuf::from("/from/metadata"),
            "config.toml's build.target-dir, via cargo metadata"
        );
        assert_eq!(
            resolve_target_dir_with(root, Some(""), &no_metadata),
            PathBuf::from("/w/root/target"),
            "an empty env value is not a redirect"
        );
        assert_eq!(
            resolve_target_dir_with(root, None, &no_metadata),
            PathBuf::from("/w/root/target"),
            "Cargo's default is the last resort"
        );
    }

    #[test]
    fn redirect_precheck_skips_the_cargo_invocation_when_nothing_can_redirect() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("workspace");
        let empty_home = tmp.path().canonicalize().unwrap().join("cargo-home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&empty_home).unwrap();

        // No manifest at all.
        assert!(!redirect_possible_with(&root, None, Some(&empty_home)));

        // A manifest, but no config anywhere mentions target-dir.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(!redirect_possible_with(&root, None, Some(&empty_home)));

        // The env override alone is enough, manifest or not.
        assert!(redirect_possible_with(&root, Some("/elsewhere"), Some(&empty_home)));
        assert!(!redirect_possible_with(&root, Some(""), Some(&empty_home)));

        // A workspace-local .cargo/config.toml that redirects.
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"/volume/cargo-target\"\n",
        )
        .unwrap();
        assert!(redirect_possible_with(&root, None, Some(&empty_home)));
        std::fs::remove_file(root.join(".cargo/config.toml")).unwrap();
        assert!(!redirect_possible_with(&root, None, Some(&empty_home)));

        // ...and one in $CARGO_HOME — the shape behind this issue.
        std::fs::write(
            empty_home.join("config.toml"),
            "[build]\ntarget-dir = \"/volume/cargo-target\"\n",
        )
        .unwrap();
        assert!(redirect_possible_with(&root, None, Some(&empty_home)));
    }

    /// Anti-drift: this module and `scripts/cargo-target-dir.sh` must resolve
    /// identically, because `worktree.sh remove` and the daemon reaper both
    /// decide what to DELETE from their answers. Skipped when the standalone
    /// script is absent (a consumer-repo checkout of the crate).
    #[test]
    fn resolution_matches_the_standalone_bash_resolver() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join("scripts/cargo-target-dir.sh"));
        let Some(script) = script.filter(|p| p.is_file()) else {
            eprintln!("skipping: scripts/cargo-target-dir.sh not present");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let ask_script = |env_value: Option<&str>| -> String {
            let mut cmd = Command::new("bash");
            cmd.arg(&script).arg(&root);
            match env_value {
                Some(v) => {
                    cmd.env("CARGO_TARGET_DIR", v);
                }
                None => {
                    cmd.env_remove("CARGO_TARGET_DIR");
                }
            }
            let out = cmd.output().expect("the bash resolver ran");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        for env_value in [Some("/abs/target-7239"), Some("rel-target-7239")] {
            let ours = resolve_target_dir_with(&root, env_value, &|_| None);
            assert_eq!(
                ours.to_string_lossy(),
                ask_script(env_value),
                "resolver drift for CARGO_TARGET_DIR={env_value:?}"
            );
        }
        // No manifest under `root`, so both fall through to `<root>/target`
        // without a network call or a real build.
        let ours = resolve_target_dir_with(&root, None, &cargo_metadata_target_directory);
        assert_eq!(
            ours.to_string_lossy(),
            ask_script(None),
            "resolver drift for the unredirected default"
        );
    }
}
