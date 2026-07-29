//! Machine-level workspace registry (Issue #3926 — phase 1 of #3835).
//!
//! The `loom-daemon` is a **one-per-machine** process: its resources (the token
//! pool at `~/.loom/tokens/`, the concurrency budget, the singleton socket at
//! `~/.loom/loom-daemon.sock`) are all machine-level. To run Loom autonomously
//! across several repos we must NOT spin up one daemon per repo — that fragments
//! the shared token budget. Instead the one daemon manages a **registry of
//! repos**.
//!
//! This module owns that registry: a small JSON file at
//! `~/.loom/workspaces.json` listing the managed repo roots (each with optional
//! per-repo config overrides). It is the persistence + mutation surface consumed
//! by both the `loom-daemon workspace add|remove|list` CLI and the
//! `RegisterWorkspace` / `DeregisterWorkspace` / `ListWorkspaces` IPC requests.
//! Because both surfaces read and write the same file, and downstream loops
//! (the work-finder, epic supervisor) can re-read it each tick, registry edits
//! are **hot-applied** without a daemon restart.
//!
//! ## Scope (phase 1)
//!
//! This phase delivers the registry data model, its persistence, and the
//! register/deregister/list surface, plus the backward-compatible resolution
//! helper ([`WorkspaceRegistry::effective_roots`]) that later phases consume:
//! with zero registered workspaces the daemon falls back to a single cwd
//! workspace, so behavior matches the pre-registry single-workspace daemon
//! byte-for-byte. The multi-repo work-finder / epic-supervisor integration,
//! `(repo, issue)`-keyed dispatch, and the global-budget/isolation/status work
//! are explicit follow-ups (see the issue's decomposition note).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Environment override for the registry file location (mirrors
/// `LOOM_SOCKET_PATH`). When set, both the CLI and the daemon read/write the
/// registry there instead of `~/.loom/workspaces.json`. Primarily a test seam,
/// but also lets an operator point several tools at an alternate registry.
pub const REGISTRY_PATH_ENV: &str = "LOOM_WORKSPACES_PATH";

/// Current on-disk schema version. Bump only on a breaking layout change; the
/// loader tolerates a missing/older `version` for forward compatibility.
pub const REGISTRY_VERSION: u32 = 1;

/// Default per-workspace dispatch priority (Issue #3946). Lower = higher
/// priority. An entry with no explicit `priority` — including every pre-#3946
/// registry file — parses as this value, so existing registries are unaffected
/// (all repos share one tier and ordering reduces to the pre-#3946 behavior).
pub const DEFAULT_WORKSPACE_PRIORITY: u32 = 100;

/// serde `default` provider for [`Workspace::priority`] / [`crate::types::RepoStatus`].
#[must_use]
pub fn default_priority() -> u32 {
    DEFAULT_WORKSPACE_PRIORITY
}

/// A single managed workspace (repo) entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Absolute, normalized repo root. This is the canonical key — two entries
    /// with the same `root` are deduplicated on `add`.
    pub root: PathBuf,
    /// Cross-repo dispatch priority tier (Issue #3946): **lower = higher
    /// priority**, default [`DEFAULT_WORKSPACE_PRIORITY`] (100). The autonomous
    /// work-finder and epic supervisor order candidates by this ascending, so a
    /// tool repo pinned to `0` outranks a product repo left at the default. A
    /// missing `priority` — including every pre-#3946 registry file — parses as
    /// the default via `#[serde(default)]`, keeping old registries byte-for-byte
    /// compatible.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Optional per-repo config overrides, stored verbatim as opaque JSON.
    /// Phase 1 persists and round-trips these but does not interpret them;
    /// later phases layer them over the repo's `.loom/config.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_overrides: Option<serde_json::Value>,
}

/// The machine-level set of managed workspaces, persisted at
/// `~/.loom/workspaces.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRegistry {
    /// On-disk schema version.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Managed workspaces, in insertion order.
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
}

fn default_version() -> u32 {
    REGISTRY_VERSION
}

impl Default for WorkspaceRegistry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            workspaces: Vec::new(),
        }
    }
}

/// Outcome of an [`WorkspaceRegistry::add`] call — distinguishes a genuine
/// insertion from a no-op re-register so the CLI/IPC can report accurately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddOutcome {
    /// The workspace was newly inserted.
    Added {
        /// The normalized root actually stored.
        canonical: PathBuf,
        /// Whether the directory looks like a Loom-managed repo (has `.git`
        /// and/or `.loom`). `false` is a soft warning, not a rejection — a
        /// freshly-cloned repo may be initialized later.
        looks_like_workspace: bool,
    },
    /// A workspace with this normalized root was already registered (no-op).
    AlreadyPresent {
        /// The normalized root that matched.
        canonical: PathBuf,
    },
}

/// Resolve the registry file path: honour [`REGISTRY_PATH_ENV`] first, else
/// `~/.loom/workspaces.json`.
pub fn default_registry_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(REGISTRY_PATH_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".loom").join("workspaces.json"))
}

/// Normalize a workspace path to an absolute, canonical form used as the dedup
/// key. Prefers [`std::fs::canonicalize`] (resolves symlinks + `..`), but for a
/// path that no longer exists (e.g. deregistering a removed repo) falls back to
/// absolutizing against the current directory without touching the filesystem.
pub fn normalize_path(input: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(input) {
        return canon;
    }
    if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(input))
            .unwrap_or_else(|_| input.to_path_buf())
    }
}

/// Whether `root` looks like a Loom-managed repo: it has a `.git` entry
/// (git repo) and/or a `.loom` directory. Used only to emit a soft warning on
/// `add`, never to reject.
fn looks_like_workspace(root: &Path) -> bool {
    root.join(".git").exists() || root.join(".loom").exists()
}

/// Resolve the **client-side** `--workspace` default for the `loom-daemon
/// dispatch` CLI (Issue #4299): if `cwd` falls under (or exactly at) a
/// registered workspace root, return that root so `dispatch` run from inside a
/// registered repo targets it — fixing the observed case where the client's
/// own cwd made no difference to dispatch (the daemon cannot see the CLI's
/// cwd, so this must be resolved client-side, before the request is built).
///
/// Pure and side-effect-free: takes the already-loaded `registry` and an
/// already-resolved `cwd` rather than performing I/O itself, so it is
/// unit-testable without touching the filesystem or environment. Returns
/// `None` when `cwd` is not under any registered root — the daemon's own
/// [`WorkspaceRegistry::resolve_dispatch_root`] then applies for the
/// explicit-param-absent case.
///
/// When more than one registered root would match (nested workspaces), the
/// **longest** (most specific) matching root wins.
#[must_use]
pub fn resolve_client_workspace_default(
    cwd: &Path,
    registry: &WorkspaceRegistry,
) -> Option<PathBuf> {
    let cwd = normalize_path(cwd);
    registry
        .workspaces
        .iter()
        .map(|w| &w.root)
        .filter(|root| cwd.starts_with(root.as_path()))
        .max_by_key(|root| root.as_os_str().len())
        .cloned()
}

impl WorkspaceRegistry {
    /// Load the registry from `path`. A missing file yields an empty registry
    /// (the common first-run case). A present-but-unparseable file is an error
    /// so a corrupted registry is loud rather than silently reset.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                if contents.trim().is_empty() {
                    return Ok(Self::default());
                }
                let registry: Self = serde_json::from_str(&contents)
                    .with_context(|| format!("parsing workspace registry at {}", path.display()))?;
                Ok(registry)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => {
                Err(e).with_context(|| format!("reading workspace registry at {}", path.display()))
            }
        }
    }

    /// Load from the default registry path ([`default_registry_path`]).
    pub fn load_default() -> Result<Self> {
        Self::load(&default_registry_path()?)
    }

    /// Persist the registry to `path` atomically (write to a sibling temp file,
    /// then rename) so a concurrent reader never observes a half-written file.
    /// Creates the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating registry dir {}", parent.display()))?;
        }
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');

        // Temp file in the same directory guarantees the rename is atomic
        // (same filesystem). Include the PID to avoid collisions between
        // concurrent writers.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing temp registry {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Persist to the default registry path.
    pub fn save_default(&self) -> Result<()> {
        self.save(&default_registry_path()?)
    }

    /// Whether a workspace with the given (already-normalized) root is present.
    #[must_use]
    pub fn contains(&self, canonical: &Path) -> bool {
        self.workspaces.iter().any(|w| w.root == canonical)
    }

    /// Register a workspace at the default priority
    /// ([`DEFAULT_WORKSPACE_PRIORITY`]). Thin wrapper over
    /// [`add_with_priority`](Self::add_with_priority) preserving the pre-#3946
    /// signature for existing callers.
    pub fn add(
        &mut self,
        root: &Path,
        config_overrides: Option<serde_json::Value>,
    ) -> Result<AddOutcome> {
        self.add_with_priority(root, config_overrides, DEFAULT_WORKSPACE_PRIORITY)
    }

    /// Register a workspace with an explicit dispatch `priority` (Issue #3946;
    /// lower = higher priority). Normalizes `root`, validates it exists and is a
    /// directory, and deduplicates on the normalized path. Idempotent: a
    /// re-register returns [`AddOutcome::AlreadyPresent`] without mutating (use
    /// [`set_priority`](Self::set_priority) to change an already-registered
    /// repo's tier).
    ///
    /// `config_overrides` is stored verbatim (only applied on a genuine insert;
    /// a re-register does not overwrite existing overrides — remove then re-add
    /// to change them).
    pub fn add_with_priority(
        &mut self,
        root: &Path,
        config_overrides: Option<serde_json::Value>,
        priority: u32,
    ) -> Result<AddOutcome> {
        let canonical = normalize_path(root);

        let meta = std::fs::metadata(&canonical)
            .with_context(|| format!("workspace path does not exist: {}", canonical.display()))?;
        if !meta.is_dir() {
            return Err(anyhow!("workspace path is not a directory: {}", canonical.display()));
        }

        if self.contains(&canonical) {
            return Ok(AddOutcome::AlreadyPresent { canonical });
        }

        let looks_like = looks_like_workspace(&canonical);
        self.workspaces.push(Workspace {
            root: canonical.clone(),
            priority,
            config_overrides,
        });
        Ok(AddOutcome::Added {
            canonical,
            looks_like_workspace: looks_like,
        })
    }

    /// Set the dispatch priority of an already-registered workspace (Issue
    /// #3946). Normalizes `root` and updates the matching entry's `priority`,
    /// returning `true` when an entry was updated, `false` when no matching
    /// workspace is registered (a no-op — the caller reports it).
    pub fn set_priority(&mut self, root: &Path, priority: u32) -> bool {
        let canonical = normalize_path(root);
        if let Some(ws) = self.workspaces.iter_mut().find(|w| w.root == canonical) {
            ws.priority = priority;
            true
        } else {
            false
        }
    }

    /// The dispatch priority of the workspace whose normalized root is exactly
    /// `root` (Issue #3946). `root` is expected to already be normalized — the
    /// roots returned by [`effective_roots`](Self::effective_roots) /
    /// [`roots`](Self::roots) are. A root not present in the registry (e.g. the
    /// empty-registry cwd fallback) resolves to [`DEFAULT_WORKSPACE_PRIORITY`].
    #[must_use]
    pub fn priority_of(&self, root: &Path) -> u32 {
        self.workspaces
            .iter()
            .find(|w| w.root == root)
            .map_or(DEFAULT_WORKSPACE_PRIORITY, |w| w.priority)
    }

    /// Deregister a workspace by root. Normalizes `root` and removes the
    /// matching entry. Returns `true` if an entry was removed, `false` if no
    /// matching workspace was registered (a no-op success).
    pub fn remove(&mut self, root: &Path) -> bool {
        let canonical = normalize_path(root);
        let before = self.workspaces.len();
        self.workspaces.retain(|w| w.root != canonical);
        self.workspaces.len() != before
    }

    /// The registered workspace roots, in insertion order.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.workspaces.iter().map(|w| w.root.clone()).collect()
    }

    /// Backward-compatible resolution of the workspaces the daemon should
    /// operate on. When the registry is **empty**, fall back to a single
    /// workspace at `cwd_fallback` — this is what preserves the pre-registry
    /// single-workspace behavior byte-for-byte (a daemon with no registry file
    /// behaves exactly as it did before #3926). When one or more workspaces are
    /// registered, they are the authoritative set and the cwd fallback is
    /// ignored.
    #[must_use]
    pub fn effective_roots(&self, cwd_fallback: &Path) -> Vec<PathBuf> {
        if self.workspaces.is_empty() {
            vec![cwd_fallback.to_path_buf()]
        } else {
            self.roots()
        }
    }

    /// Resolve the default target workspace for an **explicit-param-absent**
    /// `DispatchSweep` request (Issue #4299) — deterministic local resolution
    /// from registry state, never a forge probe (issue numbers are per-repo, so
    /// "which repo owns issue N" is ill-defined without an explicit target).
    ///
    /// `seeded_default` is the daemon's seeded default workspace (its own cwd
    /// at startup, or `LOOM_WORKSPACE`) — **not required to be pre-normalized**;
    /// this normalizes it internally before comparing against registry entries
    /// (which are always stored normalized).
    ///
    /// Deliberately returns [`DispatchRootResolution::SeededDefault`] as a
    /// distinct marker rather than the seeded path itself: the daemon's real
    /// default registry is keyed in the [`crate::workspace_pool::WorkspacePool`]
    /// by the caller's own (possibly-unnormalized) `sweep_workspace` value, so a
    /// caller that re-derives "is this the default?" via path *equality*
    /// against a freshly-normalized copy can silently mismatch on a symlinked
    /// tempdir/mount and re-provision a **second, distinct** registry instance
    /// for the same logical directory — orphaning the real default's reaper,
    /// journal, and in-memory dedup state. Returning a marker instead makes
    /// "use the literal seeded default registry" structurally unambiguous.
    ///
    /// Precedence, in order:
    /// 1. Registry **empty** -> the seeded default (byte-for-byte pre-registry
    ///    behavior — mirrors [`effective_roots`](Self::effective_roots)).
    /// 2. Seeded default **is registered** -> the seeded default. Back-compat is
    ///    load-bearing here: existing multi-workspace hosts run the daemon from
    ///    a registered workspace, and any other outcome would break every bare
    ///    `loom-daemon dispatch <N>` on them.
    /// 3. Seeded default **not registered**, exactly **one** registered
    ///    workspace -> that workspace (the single-registration Linux-worker
    ///    case this issue exists to fix).
    /// 4. Seeded default **not registered**, **multiple** registered
    ///    workspaces -> [`DispatchRootResolution::Ambiguous`], never a silent
    ///    cwd fallback.
    #[must_use]
    pub fn resolve_dispatch_root(&self, seeded_default: &Path) -> DispatchRootResolution {
        if self.workspaces.is_empty() {
            return DispatchRootResolution::SeededDefault;
        }

        let normalized_default = normalize_path(seeded_default);
        if self.contains(&normalized_default) {
            return DispatchRootResolution::SeededDefault;
        }

        let roots = self.roots();
        if roots.len() == 1 {
            return DispatchRootResolution::Registered(
                roots.into_iter().next().unwrap_or(normalized_default),
            );
        }

        DispatchRootResolution::Ambiguous { registered: roots }
    }
}

/// Outcome of [`WorkspaceRegistry::resolve_dispatch_root`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchRootResolution {
    /// Use the daemon's own seeded default registry as-is — either the
    /// registry is empty (pre-registry back-compat) or the seeded default is
    /// itself a registered workspace. The caller must dispatch through its
    /// existing default registry instance, never re-provision one.
    SeededDefault,
    /// Use this other, specific registered workspace root (provisioned via the
    /// workspace pool).
    Registered(PathBuf),
    /// The daemon's seeded default isn't registered and more than one
    /// workspace is registered — no safe default; the caller must ask for an
    /// explicit `--workspace`/`workspace_root`. Carries the full registered set
    /// so the caller can list it in the error.
    Ambiguous {
        /// The currently-registered workspace roots, in registry order.
        registered: Vec<PathBuf>,
    },
}

/// Skip registered roots whose directory no longer exists on disk (#4326),
/// warning once per missing period rather than once per tick.
///
/// Shared by [`crate::work_finder`] (single- and multi-workspace loops) and
/// [`crate::role_runner::spawn_multi_role_task`] (#4349) — both re-read
/// [`WorkspaceRegistry::effective_roots`] every tick and must apply the same
/// missing-root hygiene before dispatching against the result, rather than
/// each maintaining its own drifting copy of this predicate.
///
/// Deliberately does **not** live inside [`WorkspaceRegistry::effective_roots`]
/// — that helper's empty-registry branch falls back to the daemon's cwd, and
/// dropping missing roots *inside* it would make an all-roots-missing
/// registry indistinguishable from an empty one, silently redirecting
/// dispatch into the daemon's own cwd. Filtering here instead means an
/// all-missing registry yields zero roots for this tick — no dispatch, no
/// cwd fallback — while the dangling entries stay registered so
/// `loom-daemon status` can flag them and an operator can `workspace remove`
/// them. This is warn-and-skip, **never auto-remove**: a root can be
/// transiently absent (an unmounted network volume, an in-progress restore),
/// and auto-deregistering would destroy operator state on a transient
/// condition.
///
/// `warned` carries the set of roots currently known-missing across ticks so
/// a long-dangling entry logs once on first-seen-missing (not once per tick),
/// and — if it later reappears and then disappears again — re-warns instead
/// of staying silent forever.
pub fn filter_missing_roots(roots: Vec<PathBuf>, warned: &mut HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut existing = Vec::with_capacity(roots.len());
    let mut still_missing: HashSet<PathBuf> = HashSet::new();
    for root in roots {
        if root.is_dir() {
            existing.push(root);
        } else {
            if !warned.contains(&root) {
                log::warn!(
                    "workspace_registry: registered workspace root {} does not exist on disk; \
                     skipping it for this tick (entry stays registered — run \
                     `loom-daemon workspace remove {}` to deregister it if this is \
                     permanent, or `loom-daemon status` to confirm)",
                    root.display(),
                    root.display()
                );
            }
            still_missing.insert(root);
        }
    }
    *warned = still_missing;
    existing
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        let reg = WorkspaceRegistry::load(&path).unwrap();
        assert!(reg.workspaces.is_empty());
        assert_eq!(reg.version, REGISTRY_VERSION);
    }

    #[test]
    fn load_empty_file_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, "   \n").unwrap();
        let reg = WorkspaceRegistry::load(&path).unwrap();
        assert!(reg.workspaces.is_empty());
    }

    #[test]
    fn load_corrupt_file_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(WorkspaceRegistry::load(&path).is_err());
    }

    #[test]
    fn add_then_list_roundtrip() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        let outcome = reg.add(&repo, None).unwrap();
        match outcome {
            AddOutcome::Added { canonical, .. } => {
                assert_eq!(canonical, std::fs::canonicalize(&repo).unwrap());
            }
            AddOutcome::AlreadyPresent { .. } => panic!("expected Added"),
        }
        assert_eq!(reg.workspaces.len(), 1);
        assert!(reg.contains(&std::fs::canonicalize(&repo).unwrap()));
    }

    #[test]
    fn add_is_idempotent() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();
        let second = reg.add(&repo, None).unwrap();
        assert!(matches!(second, AddOutcome::AlreadyPresent { .. }));
        assert_eq!(reg.workspaces.len(), 1, "re-register must not duplicate");
    }

    #[test]
    fn add_dedups_via_normalization() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();
        // A path with a redundant `.` segment normalizes to the same canonical.
        let dotted = repo.join(".");
        let second = reg.add(&dotted, None).unwrap();
        assert!(matches!(second, AddOutcome::AlreadyPresent { .. }));
        assert_eq!(reg.workspaces.len(), 1);
    }

    #[test]
    fn add_nonexistent_path_errors() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let mut reg = WorkspaceRegistry::default();
        assert!(reg.add(&missing, None).is_err());
    }

    #[test]
    fn add_file_not_dir_errors() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "hi").unwrap();
        let mut reg = WorkspaceRegistry::default();
        assert!(reg.add(&file, None).is_err());
    }

    #[test]
    fn add_reports_workspace_likeness() {
        let dir = tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let loomy = dir.path().join("loomy");
        std::fs::create_dir_all(loomy.join(".loom")).unwrap();

        let mut reg = WorkspaceRegistry::default();
        match reg.add(&plain, None).unwrap() {
            AddOutcome::Added {
                looks_like_workspace,
                ..
            } => assert!(!looks_like_workspace),
            AddOutcome::AlreadyPresent { .. } => panic!("expected Added"),
        }
        match reg.add(&loomy, None).unwrap() {
            AddOutcome::Added {
                looks_like_workspace,
                ..
            } => assert!(looks_like_workspace),
            AddOutcome::AlreadyPresent { .. } => panic!("expected Added"),
        }
    }

    #[test]
    fn remove_present_and_absent() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();
        assert!(reg.remove(&repo), "removing a present entry returns true");
        assert!(reg.workspaces.is_empty());
        assert!(!reg.remove(&repo), "removing an absent entry returns false");
    }

    #[test]
    fn remove_of_deleted_path_still_works() {
        // A workspace whose directory has since been deleted must still be
        // deregisterable — normalize_path falls back to absolutization.
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical = std::fs::canonicalize(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.workspaces.push(Workspace {
            root: canonical.clone(),
            priority: DEFAULT_WORKSPACE_PRIORITY,
            config_overrides: None,
        });

        std::fs::remove_dir_all(&repo).unwrap();
        // Removing by the now-canonical absolute path succeeds.
        assert!(reg.remove(&canonical));
        assert!(reg.workspaces.is_empty());
    }

    #[test]
    fn save_load_roundtrip_preserves_overrides() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let path = dir.path().join("nested").join("workspaces.json");

        let overrides =
            serde_json::json!({ "autonomous": { "workFinder": { "maxConcurrent": 2 } } });
        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, Some(overrides.clone())).unwrap();
        reg.save(&path).unwrap();

        let loaded = WorkspaceRegistry::load(&path).unwrap();
        assert_eq!(loaded, reg);
        assert_eq!(loaded.workspaces[0].config_overrides, Some(overrides));
    }

    #[test]
    fn save_is_atomic_and_creates_parent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("workspaces.json");
        let reg = WorkspaceRegistry::default();
        reg.save(&path).unwrap();
        assert!(path.exists());
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file should be renamed away");
    }

    #[test]
    fn effective_roots_empty_falls_back_to_cwd() {
        let reg = WorkspaceRegistry::default();
        let cwd = PathBuf::from("/some/cwd");
        assert_eq!(reg.effective_roots(&cwd), vec![cwd.clone()]);
    }

    #[test]
    fn effective_roots_uses_registered_set() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&a, None).unwrap();
        reg.add(&b, None).unwrap();

        let ignored_cwd = PathBuf::from("/ignored");
        let roots = reg.effective_roots(&ignored_cwd);
        assert_eq!(roots.len(), 2);
        assert!(!roots.contains(&ignored_cwd));
    }

    #[test]
    #[serial_test::serial]
    fn default_registry_path_honours_env_override() {
        // Use a serial-ish approach: set + read + unset within one test.
        let dir = tempdir().unwrap();
        let custom = dir.path().join("custom-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &custom);
        let resolved = default_registry_path().unwrap();
        std::env::remove_var(REGISTRY_PATH_ENV);
        assert_eq!(resolved, custom);
    }

    #[test]
    fn version_defaults_when_absent_in_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        // Legacy/hand-written file with no `version` field.
        std::fs::write(&path, r#"{ "workspaces": [] }"#).unwrap();
        let reg = WorkspaceRegistry::load(&path).unwrap();
        assert_eq!(reg.version, REGISTRY_VERSION);
    }

    // ===================================================================
    // Priority tiers (#3946)
    // ===================================================================

    #[test]
    fn add_defaults_to_default_priority() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();
        assert_eq!(reg.workspaces[0].priority, DEFAULT_WORKSPACE_PRIORITY);
    }

    #[test]
    fn add_with_priority_stores_and_looks_up() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical = std::fs::canonicalize(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add_with_priority(&repo, None, 0).unwrap();
        assert_eq!(reg.workspaces[0].priority, 0);
        assert_eq!(reg.priority_of(&canonical), 0);
    }

    #[test]
    fn priority_of_unregistered_root_is_default() {
        let reg = WorkspaceRegistry::default();
        assert_eq!(reg.priority_of(Path::new("/not/registered")), DEFAULT_WORKSPACE_PRIORITY);
    }

    #[test]
    fn set_priority_updates_existing_and_reports_missing() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical = std::fs::canonicalize(&repo).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();
        assert!(reg.set_priority(&repo, 1), "updating a present entry returns true");
        assert_eq!(reg.priority_of(&canonical), 1);

        assert!(
            !reg.set_priority(Path::new("/absent"), 5),
            "updating an absent entry returns false"
        );
    }

    #[test]
    fn priority_survives_save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let path = dir.path().join("workspaces.json");

        let mut reg = WorkspaceRegistry::default();
        reg.add_with_priority(&repo, None, 3).unwrap();
        reg.save(&path).unwrap();

        let loaded = WorkspaceRegistry::load(&path).unwrap();
        assert_eq!(loaded.workspaces[0].priority, 3);
        assert_eq!(loaded, reg);
    }

    #[test]
    fn legacy_entry_without_priority_parses_as_default() {
        // Backward compatibility (#3946 acceptance): a pre-#3946 registry file
        // whose workspace object has NO `priority` key must load as the default
        // priority, not fail to parse.
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        let repo = dir.path().join("legacy-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root_json = serde_json::to_string(&repo).unwrap();
        std::fs::write(
            &path,
            format!(r#"{{ "version": 1, "workspaces": [ {{ "root": {root_json} }} ] }}"#),
        )
        .unwrap();

        let reg = WorkspaceRegistry::load(&path).unwrap();
        assert_eq!(reg.workspaces.len(), 1);
        assert_eq!(
            reg.workspaces[0].priority, DEFAULT_WORKSPACE_PRIORITY,
            "an entry with no `priority` parses as the default (backward compat)"
        );
    }

    // ===================================================================
    // Dispatch-path workspace resolution (#4299)
    // ===================================================================

    #[test]
    fn resolve_dispatch_root_empty_registry_uses_seeded_default() {
        let reg = WorkspaceRegistry::default();
        let seeded = PathBuf::from("/some/cwd/wherever");
        assert_eq!(
            reg.resolve_dispatch_root(&seeded),
            DispatchRootResolution::SeededDefault,
            "empty registry preserves byte-for-byte pre-registry (cwd) behavior"
        );
    }

    #[test]
    fn resolve_dispatch_root_seeded_default_registered_wins() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&a, None).unwrap();
        reg.add(&b, None).unwrap();

        assert_eq!(
            reg.resolve_dispatch_root(&a),
            DispatchRootResolution::SeededDefault,
            "existing multi-workspace hosts (daemon cwd == a registered root) keep the \
             same default with no new flags"
        );
    }

    #[test]
    fn resolve_dispatch_root_single_unregistered_seed_targets_the_registration() {
        // The Linux worker-host shape this issue exists to fix: daemon cwd is
        // the machine checkout (unregistered), exactly one workspace (anvil) is
        // registered.
        let dir = tempdir().unwrap();
        let machine_checkout = dir.path().join("machine-checkout");
        let anvil = dir.path().join("anvil");
        std::fs::create_dir_all(&machine_checkout).unwrap();
        std::fs::create_dir_all(&anvil).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&anvil, None).unwrap();

        let canonical_anvil = std::fs::canonicalize(&anvil).unwrap();
        assert_eq!(
            reg.resolve_dispatch_root(&machine_checkout),
            DispatchRootResolution::Registered(canonical_anvil)
        );
    }

    #[test]
    fn resolve_dispatch_root_multiple_unregistered_seed_is_ambiguous() {
        let dir = tempdir().unwrap();
        let machine_checkout = dir.path().join("machine-checkout");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&machine_checkout).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&a, None).unwrap();
        reg.add(&b, None).unwrap();

        match reg.resolve_dispatch_root(&machine_checkout) {
            DispatchRootResolution::Ambiguous { registered } => {
                assert_eq!(registered.len(), 2, "ambiguity error names every registered root");
                assert!(registered.contains(&std::fs::canonicalize(&a).unwrap()));
                assert!(registered.contains(&std::fs::canonicalize(&b).unwrap()));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn resolve_client_workspace_default_matches_cwd_under_root() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("subdir");
        std::fs::create_dir_all(&nested).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();

        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(
            resolve_client_workspace_default(&repo, &reg),
            Some(canonical_repo.clone()),
            "cwd exactly at the registered root matches"
        );
        assert_eq!(
            resolve_client_workspace_default(&nested, &reg),
            Some(canonical_repo),
            "cwd nested under the registered root matches"
        );
    }

    #[test]
    fn resolve_client_workspace_default_no_match_outside_any_root() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&repo, None).unwrap();

        assert_eq!(resolve_client_workspace_default(&elsewhere, &reg), None);
    }

    #[test]
    fn resolve_client_workspace_default_empty_registry_is_none() {
        let reg = WorkspaceRegistry::default();
        let dir = tempdir().unwrap();
        assert_eq!(resolve_client_workspace_default(dir.path(), &reg), None);
    }

    #[test]
    fn resolve_client_workspace_default_prefers_most_specific_nested_match() {
        let dir = tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();

        let mut reg = WorkspaceRegistry::default();
        reg.add(&outer, None).unwrap();
        reg.add(&inner, None).unwrap();

        let canonical_inner = std::fs::canonicalize(&inner).unwrap();
        assert_eq!(resolve_client_workspace_default(&inner, &reg), Some(canonical_inner));
    }

    // ===================================================================
    // Missing-root hygiene (Issue #4326; hoisted here shared for #4349)
    // ===================================================================

    #[test]
    fn test_filter_missing_roots_skips_only_the_missing_one() {
        // A registry with one existing and one missing root dispatches only
        // into the existing root — the missing one is dropped, not the whole
        // tick.
        let tmp = tempdir().unwrap();
        let existing = tmp.path().to_path_buf();
        let missing = tmp.path().join("does-not-exist");
        let mut warned = HashSet::new();

        let filtered = filter_missing_roots(vec![existing.clone(), missing.clone()], &mut warned);

        assert_eq!(filtered, vec![existing], "only the existing root survives");
        assert!(warned.contains(&missing), "the missing root is tracked as warned");
    }

    #[test]
    fn test_filter_missing_roots_all_missing_does_not_fall_back_to_cwd() {
        // The all-missing case must yield an EMPTY root list, never a silent
        // fallback to the daemon's cwd (that fallback is
        // `effective_roots`'s exclusive, empty-*registry* behavior — a
        // non-empty registry whose entries all happen to be missing on disk
        // is a distinct case and must not reuse it).
        let tmp = tempdir().unwrap();
        let missing_a = tmp.path().join("gone-a");
        let missing_b = tmp.path().join("gone-b");
        let mut warned = HashSet::new();

        let filtered = filter_missing_roots(vec![missing_a, missing_b], &mut warned);

        assert!(filtered.is_empty(), "an all-missing registry yields zero roots, not cwd");
        assert_eq!(warned.len(), 2);
    }

    #[test]
    fn test_filter_missing_roots_warns_once_then_recovers() {
        // The `warned` set only ever tracks roots missing on the *current*
        // call — a caller that re-checks each tick (as the work-finder loop
        // does) naturally re-warns if a root disappears again after
        // recovering, and stays silent tick-over-tick while it remains
        // missing (the loop only logs for roots newly inserted into
        // `warned`, exercised by the loop itself, not this pure-function
        // test — this test just verifies the recovery bookkeeping).
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("flaky");
        let mut warned = HashSet::new();

        // First call: missing.
        let filtered = filter_missing_roots(vec![root.clone()], &mut warned);
        assert!(filtered.is_empty());
        assert!(warned.contains(&root));

        // Root reappears (e.g. a remounted volume).
        std::fs::create_dir_all(&root).unwrap();
        let filtered = filter_missing_roots(vec![root.clone()], &mut warned);
        assert_eq!(filtered, vec![root.clone()]);
        assert!(warned.is_empty(), "a recovered root is no longer tracked as missing");

        // Root disappears again — should be treated as newly-missing (i.e.
        // would re-warn), not silently skipped as "already known".
        std::fs::remove_dir_all(&root).unwrap();
        let filtered = filter_missing_roots(vec![root.clone()], &mut warned);
        assert!(filtered.is_empty());
        assert!(warned.contains(&root));
    }

    #[test]
    fn test_filter_missing_roots_empty_input_returns_empty() {
        let mut warned = HashSet::new();
        assert!(filter_missing_roots(vec![], &mut warned).is_empty());
        assert!(warned.is_empty());
    }
}
