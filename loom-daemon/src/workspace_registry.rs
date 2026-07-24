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
use std::path::{Path, PathBuf};

/// Environment override for the registry file location (mirrors
/// `LOOM_SOCKET_PATH`). When set, both the CLI and the daemon read/write the
/// registry there instead of `~/.loom/workspaces.json`. Primarily a test seam,
/// but also lets an operator point several tools at an alternate registry.
pub const REGISTRY_PATH_ENV: &str = "LOOM_WORKSPACES_PATH";

/// Current on-disk schema version. Bump only on a breaking layout change; the
/// loader tolerates a missing/older `version` for forward compatibility.
pub const REGISTRY_VERSION: u32 = 1;

/// A single managed workspace (repo) entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Absolute, normalized repo root. This is the canonical key — two entries
    /// with the same `root` are deduplicated on `add`.
    pub root: PathBuf,
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

    /// Register a workspace. Normalizes `root`, validates it exists and is a
    /// directory, and deduplicates on the normalized path. Idempotent: a
    /// re-register returns [`AddOutcome::AlreadyPresent`] without mutating.
    ///
    /// `config_overrides` is stored verbatim (only applied on a genuine insert;
    /// a re-register does not overwrite existing overrides — remove then re-add
    /// to change them).
    pub fn add(
        &mut self,
        root: &Path,
        config_overrides: Option<serde_json::Value>,
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
            config_overrides,
        });
        Ok(AddOutcome::Added {
            canonical,
            looks_like_workspace: looks_like,
        })
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
}
