//! Per-worktree cargo target dirs — the daemon's half (issue #8458).
//!
//! # What the scheme is
//!
//! Cargo keys a *workspace* crate's artifacts (and its incremental session) by
//! the crate's absolute source path, so two Loom worktrees never share
//! workspace-crate build output even inside one shared target dir. The sharing
//! therefore buys nothing for the crates Loom actually rebuilds, while costing
//! two things measured in issue #8453: unbounded growth (213 GB of
//! `debug/incremental/` + 231 GB of `debug/deps/` on one host, none of it
//! reachable from any live worktree), and **wrong test results** — Cargo uplifts
//! the final binary to one un-hashed path, `<target>/debug/loom-daemon`,
//! overwritten by whichever worktree built last, and integration tests execute
//! that path.
//!
//! The fix gives each worktree `<root>/wt/<worktree name>` under the otherwise
//! shared root. `worktree.sh` provisions it (bash side,
//! `loom-daemon cargo-target-dir provision`) and records it in a
//! `.loom-cargo-target-dir` marker file **inside** the worktree.
//!
//! # Why the daemon needs to know
//!
//! The daemon's own removal paths — `loom-daemon clean` and the periodic
//! worktree reaper (#4876), which is what catches a worktree whose PR was merged
//! on a *different* host — reclaim a redirected target dir through
//! [`super::plan_reclaim`]. Without the marker they resolve a per-worktree
//! worktree to the **shared root** (via `cargo metadata`, which applies the
//! host's `~/.cargo/config.toml`), and gate 2f then correctly refuses to delete
//! that machine-global path. The per-worktree dir would leak forever, which is
//! the same unbounded-growth failure one level down.
//!
//! So resolution consults the marker first. Everything else — every refusal
//! gate, the sharing scan, the live-process gate — is unchanged and keeps
//! applying to whatever the marker names.
//!
//! # Why a marker file and not `.cargo/config.toml`
//!
//! #7239's attribution rule is that only a redirect derived from the worktree
//! itself can prove a directory belongs to it, and it names `build.target-dir`
//! in a `.cargo/config.toml` inside the worktree as the way that happens. That
//! vehicle is unavailable in a repo that *tracks* `.cargo/config.toml` — Loom
//! itself does — because the redirect would then show up as a modified tracked
//! file in every worktree. The marker is in the worktree, so it is
//! worktree-derived evidence in exactly the sense #7239 requires, and it is not
//! a Cargo config, so it touches no tracked state. Cargo learns the redirect
//! from `CARGO_TARGET_DIR`, exported by the spawn path.
//!
//! Kept deliberately parallel to `defaults/scripts/lib/cargo-target-dir.sh`
//! § "Per-worktree target dirs", for the reason the parent module's header
//! gives: the daemon reaps worktrees in repos where Loom's bash library is not
//! installed at all.

use std::path::{Path, PathBuf};

/// Loom runtime marker recording a worktree's provisioned target dir. Gitignored
/// via [`crate::init::post_init::EPHEMERAL_PATTERNS`], in the same family as
/// `.loom-managed`.
pub const MARKER_FILE: &str = ".loom-cargo-target-dir";

/// The one path component separating per-worktree dirs from anything else under
/// the shared root. Load-bearing: it is half of the structural attribution
/// check in [`is_attributable`].
pub const SUBDIR: &str = "wt";

/// Does `candidate` carry the Loom per-worktree shape **for `worktree_path`** —
/// an absolute path of at least three components ending in
/// `/wt/<basename of worktree_path>`?
///
/// Purely structural: no disk access, so it is still answerable after the
/// worktree has been removed, which is when [`super::plan_reclaim`]'s gates run.
///
/// This predicate is what licenses the sharing-scan relaxation in
/// [`super::plan_reclaim`], so it is written to be un-widenable. Requiring the
/// leaf to equal the worktree's own directory name ties the directory to exactly
/// one worktree by name: a machine-global root (`/big/cargo-target`) can never
/// satisfy it, and a truncated or hand-edited marker can only ever name
/// `<something>/wt/<this worktree's own name>` — never the shared root, never a
/// parent of it, never a sibling worktree's directory.
#[must_use]
pub fn is_attributable(worktree_path: &Path, candidate: &Path) -> bool {
    let Some(name) = worktree_path.file_name() else {
        return false;
    };
    if !candidate.is_absolute() {
        return false;
    }
    // `/wt/<name>` alone is two components past the root; require a real root
    // above it so the "suspiciously shallow path" family is unreachable here.
    if candidate.components().count() < 4 {
        return false;
    }
    let mut parts = candidate.components().rev();
    let Some(leaf) = parts.next() else {
        return false;
    };
    let Some(parent) = parts.next() else {
        return false;
    };
    leaf.as_os_str() == name && parent.as_os_str() == SUBDIR
}

/// Derive `<target_root>/wt/<worktree_name>`.
///
/// **Idempotent**: when `target_root` is already this worktree's per-worktree dir
/// it is returned unchanged rather than nested a second level. That case is
/// routine — the spawn path exports `CARGO_TARGET_DIR` for the whole sweep, so a
/// later re-resolution sees the per-worktree value as its "root".
#[must_use]
pub fn dir_for(target_root: &Path, worktree_name: &str) -> PathBuf {
    if is_attributable(Path::new(worktree_name), target_root) {
        return target_root.to_path_buf();
    }
    target_root.join(SUBDIR).join(worktree_name)
}

/// The target dir recorded in `worktree_path`'s marker, or `None` when there is
/// no usable one. **Must be called while the worktree is still on disk.**
///
/// Every failure mode — absent marker, empty marker, a relative or
/// non-per-worktree-shaped value, a tree with no Cargo manifest — yields `None`,
/// so a corrupt marker degrades to "no per-worktree redirect" (pre-#8458
/// behavior) rather than to a path this module would then act on.
///
/// The manifest requirement mirrors [`super::redirect_possible_with`]'s first
/// test on purpose: a tree cargo never built in must resolve to its own
/// in-worktree `target/`, which is the #7239 regression that ordering pins.
#[must_use]
pub fn marker_value(worktree_path: &Path) -> Option<PathBuf> {
    marker_value_with(worktree_path, &|p| std::fs::read_to_string(p).ok(), &|p| p.is_file())
}

/// [`marker_value`] with its two filesystem reads injected, so the whole
/// validation chain is unit-testable without a fixture tree.
#[must_use]
pub fn marker_value_with(
    worktree_path: &Path,
    read: &dyn Fn(&Path) -> Option<String>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if !is_file(&worktree_path.join("Cargo.toml")) {
        return None;
    }
    let body = read(&worktree_path.join(MARKER_FILE))?;
    let line = body.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(line.trim_end_matches('/'));
    is_attributable(worktree_path, &candidate).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wt(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn the_provisioned_shape_is_attributable() {
        assert!(is_attributable(
            &wt("/repo/.loom/worktrees/issue-8458"),
            Path::new("/big/cargo-target/wt/issue-8458")
        ));
    }

    #[test]
    fn a_machine_global_shared_root_is_never_attributable() {
        // The whole point: `/big/cargo-target` is the directory #8453's host
        // shares across every checkout. No relaxation may ever reach it.
        for candidate in [
            "/big/cargo-target",
            "/big/cargo-target/debug",
            "/big/cargo-target/wt",
            "/home/ubuntu/.cargo/target",
        ] {
            assert!(
                !is_attributable(&wt("/repo/.loom/worktrees/issue-8458"), Path::new(candidate)),
                "{candidate} must not be attributable"
            );
        }
    }

    #[test]
    fn another_worktrees_dir_is_not_attributable_to_this_one() {
        assert!(!is_attributable(
            &wt("/repo/.loom/worktrees/issue-8458"),
            Path::new("/big/cargo-target/wt/issue-8457")
        ));
    }

    #[test]
    fn the_wt_component_is_required() {
        assert!(!is_attributable(
            &wt("/repo/.loom/worktrees/issue-8458"),
            Path::new("/big/cargo-target/other/issue-8458")
        ));
    }

    #[test]
    fn a_relative_or_shallow_value_is_not_attributable() {
        assert!(!is_attributable(
            &wt("/repo/.loom/worktrees/issue-8458"),
            Path::new("cargo-target/wt/issue-8458")
        ));
        // `/wt/issue-8458` has no root above `wt/` — 3 components counting the
        // root prefix, which the shallow-path family must never see.
        assert!(!is_attributable(
            &wt("/repo/.loom/worktrees/issue-8458"),
            Path::new("/wt/issue-8458")
        ));
    }

    #[test]
    fn dir_for_appends_the_two_components() {
        assert_eq!(
            dir_for(Path::new("/big/cargo-target"), "issue-8458"),
            PathBuf::from("/big/cargo-target/wt/issue-8458")
        );
    }

    #[test]
    fn dir_for_does_not_nest_an_already_per_worktree_root() {
        // The spawn path exported CARGO_TARGET_DIR; worktree.sh re-resolves it
        // as its "root". Nesting here would make the marker and the env var name
        // different directories.
        let already = PathBuf::from("/big/cargo-target/wt/issue-8458");
        assert_eq!(dir_for(&already, "issue-8458"), already);
    }

    #[test]
    fn marker_is_read_when_valid() {
        let worktree = wt("/repo/.loom/worktrees/issue-8458");
        let value = marker_value_with(
            &worktree,
            &|_| Some("/big/cargo-target/wt/issue-8458\n".to_string()),
            &|_| true,
        );
        assert_eq!(value, Some(PathBuf::from("/big/cargo-target/wt/issue-8458")));
    }

    #[test]
    fn a_trailing_slash_is_normalized_away() {
        let worktree = wt("/repo/.loom/worktrees/issue-8458");
        let value = marker_value_with(
            &worktree,
            &|_| Some("/big/cargo-target/wt/issue-8458/".to_string()),
            &|_| true,
        );
        assert_eq!(value, Some(PathBuf::from("/big/cargo-target/wt/issue-8458")));
    }

    #[test]
    fn a_manifestless_tree_yields_nothing_even_with_a_marker() {
        // Mirrors `redirect_possible_with`'s manifest-first ordering: a tree
        // cargo never built in must resolve to its own in-worktree `target/`.
        let worktree = wt("/repo/.loom/worktrees/issue-8458");
        let value = marker_value_with(
            &worktree,
            &|_| Some("/big/cargo-target/wt/issue-8458".to_string()),
            &|p| !p.ends_with("Cargo.toml"),
        );
        assert_eq!(value, None);
    }

    #[test]
    fn a_marker_naming_the_shared_root_is_rejected() {
        // A corrupted/truncated marker must degrade to "no redirect", never to a
        // path the reclaim would then delete.
        let worktree = wt("/repo/.loom/worktrees/issue-8458");
        for body in ["/big/cargo-target", "", "   ", "relative/wt/issue-8458"] {
            assert_eq!(
                marker_value_with(&worktree, &|_| Some(body.to_string()), &|_| true),
                None,
                "marker body {body:?} must be rejected"
            );
        }
    }

    #[test]
    fn an_absent_marker_yields_nothing() {
        let worktree = wt("/repo/.loom/worktrees/issue-8458");
        assert_eq!(marker_value_with(&worktree, &|_| None, &|_| true), None);
    }

    // ----------------------------------------------------------------------
    // The two relaxations `is_attributable` licenses, exercised through the
    // real `plan_reclaim`. These are the cases that decide whether the scheme
    // frees any disk at all, and the case that decides whether it is safe.
    // ----------------------------------------------------------------------

    use super::super::{plan_reclaim, TargetDirOutcome, TargetDirProbes};

    struct Shape {
        _tmp: tempfile::TempDir,
        repo_root: PathBuf,
        worktree: PathBuf,
        shared_root: PathBuf,
        per_worktree: PathBuf,
    }

    /// The #8453 host: one machine-global shared root, a cargo primary checkout
    /// that resolves to it, and one worktree with its own `<root>/wt/<name>`.
    fn shape() -> Shape {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let repo_root = base.join("repo");
        let worktree = repo_root.join(".loom/worktrees/issue-8458");
        let shared_root = base.join("big/cargo-target");
        let per_worktree = shared_root.join("wt/issue-8458");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(repo_root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(worktree.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(shared_root.join("debug")).unwrap();
        std::fs::create_dir_all(per_worktree.join("debug")).unwrap();
        std::fs::write(per_worktree.join("debug/loom-daemon"), vec![0u8; 2048]).unwrap();
        Shape {
            _tmp: tmp,
            repo_root,
            worktree,
            shared_root,
            per_worktree,
        }
    }

    fn has_manifest(p: &Path) -> bool {
        p.join("Cargo.toml").is_file()
    }

    /// Reclaim `resolved`, with the primary checkout resolving to `shared_root`
    /// and the remover's ambient `CARGO_TARGET_DIR` set to `ambient`.
    fn run(s: &Shape, resolved: &Path, ambient: Option<&Path>) -> TargetDirOutcome {
        let live = vec![s.repo_root.clone(), s.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let shared = s.shared_root.clone();
        // The primary checkout builds into the shared root — via the host's
        // `~/.cargo/config.toml`, which is why it is a *containing* sharer.
        let resolve = move |_: &Path| Some(shared.clone());
        let ambient_owned = ambient.map(Path::to_path_buf);
        let machine_global = move |_: &Path| {
            ambient_owned
                .iter()
                .map(|p| (p.clone(), "the ambient CARGO_TARGET_DIR".to_string()))
                .collect::<Vec<_>>()
        };
        let holders = |_: &Path| Vec::new();
        let size_human = |_: &Path| "3.5G".to_string();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &machine_global,
            holders: &holders,
            size_human: &size_human,
            remove: &remove,
        };
        plan_reclaim(&s.repo_root, &s.worktree, resolved, true, &probes)
    }

    #[test]
    fn a_per_worktree_dir_is_reclaimable_despite_the_containing_shared_root() {
        // Before the containment relaxation this was `Shared { by: repo_root }`
        // on every single removal, so the scheme could never free a byte.
        let s = shape();
        assert_eq!(
            run(&s, &s.per_worktree, None),
            TargetDirOutcome::WouldReclaim {
                path: s.per_worktree.clone(),
                size_human: "3.5G".to_string()
            }
        );
    }

    #[test]
    fn a_per_worktree_dir_is_reclaimable_when_it_is_also_the_ambient_env_value() {
        // What the spawn path exports: the remover's own CARGO_TARGET_DIR IS
        // this worktree's per-worktree dir. Gate 2f would otherwise refuse it as
        // "machine-global", which is the documented pre-#8458 safe direction.
        let s = shape();
        let per = s.per_worktree.clone();
        assert_eq!(
            run(&s, &s.per_worktree, Some(&per)),
            TargetDirOutcome::WouldReclaim {
                path: s.per_worktree.clone(),
                size_human: "3.5G".to_string()
            }
        );
    }

    #[test]
    fn the_machine_global_shared_root_is_still_refused() {
        // THE data-loss guard (#7239). Neither relaxation may reach a path
        // without the per-worktree shape, however the removal is driven.
        let s = shape();
        let shared = s.shared_root.clone();
        let outcome = run(&s, &s.shared_root, Some(&shared));
        match outcome {
            TargetDirOutcome::Refused { path, reason } => {
                assert_eq!(path, s.shared_root);
                assert!(reason.contains("machine-global"), "unexpected reason: {reason}");
            }
            other => panic!("the shared root must be refused, got {other:?}"),
        }
        assert!(s.shared_root.is_dir(), "the shared root must still exist");
    }

    #[test]
    fn another_worktrees_per_worktree_dir_is_still_shared() {
        // The relaxation only drops CONTAINMENT. An EXACT match with a live
        // worktree's own target dir still counts, so a sibling's directory can
        // never be removed by this worktree's teardown.
        let s = shape();
        let sibling_dir = s.shared_root.join("wt/issue-8457");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        let live = vec![s.repo_root.clone(), s.worktree.clone()];
        let live_worktrees = || Some(live.clone());
        let sibling = sibling_dir.clone();
        let resolve = move |_: &Path| Some(sibling.clone());
        let holders = |_: &Path| Vec::new();
        let size_human = |_: &Path| "1G".to_string();
        let remove = |p: &Path| std::fs::remove_dir_all(p);
        let machine_global = |_: &Path| Vec::new();
        let probes = TargetDirProbes {
            live_worktrees: &live_worktrees,
            has_manifest: &has_manifest,
            resolve: &resolve,
            machine_global_target_dirs: &machine_global,
            holders: &holders,
            size_human: &size_human,
            remove: &remove,
        };
        // Ask about the SIBLING's dir while removing issue-8458: it is not
        // attributable to issue-8458, and it is a live worktree's own dir.
        let outcome = plan_reclaim(&s.repo_root, &s.worktree, &sibling_dir, true, &probes);
        assert!(
            matches!(outcome, TargetDirOutcome::Shared { .. }),
            "expected Shared, got {outcome:?}"
        );
    }
}
