//! Bound Codex per-profile session history (#12966).
//!
//! Codex appends one rollout JSONL per session under
//! `<profile>/sessions/<YYYY>/<MM>/<DD>/` and never expires it, so registered,
//! session-managed profiles under the machine-level Codex profile root grow
//! without bound (7.7 GB over 7 profiles, 71% of it `sessions/`). Codex has no
//! native retention setting (`codex --help` offers only `archive`/`delete` of
//! individual sessions), so this pass deletes rollout files older than a
//! retention floor and then the date directories it emptied.
//!
//! Deliberately narrow:
//!
//! * Only `rollout-*.jsonl` files inside `sessions/` and `archived_sessions/`
//!   are ever deleted. `auth.json`, `config.toml`, `hooks.json`,
//!   `.loom-profile.json`, `.session-managed.json`, `thread_history_1.sqlite`
//!   and every dot-dir are never visited (compacting the thread-history db is
//!   a product decision, out of scope).
//! * Pooled profiles symlink `sessions/` to the default profile's tree. Each
//!   history dir is resolved to its real target and pruned once; a target
//!   outside the profile root and the default Codex home is skipped, and a
//!   symlink met *inside* a tree is never followed.
//! * Age floor (default 30 days) is far above the longest sweep, so a live
//!   sweep or `--resume` keeps its rollout.
//!
//! Config precedence **env > config > default**:
//! `LOOM_CODEX_SESSION_REAPER` / `autonomous.codexSessionReaper.enabled`
//! (default on) and `LOOM_CODEX_SESSION_RETENTION_DAYS` /
//! `autonomous.codexSessionReaper.retentionDays` (default 30).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Env var overriding `autonomous.codexSessionReaper.enabled`.
pub const CODEX_SESSION_REAPER_ENABLE_ENV: &str = "LOOM_CODEX_SESSION_REAPER";
/// Env var overriding `autonomous.codexSessionReaper.retentionDays`.
pub const CODEX_SESSION_RETENTION_DAYS_ENV: &str = "LOOM_CODEX_SESSION_RETENTION_DAYS";
/// Default age floor for a rollout file.
pub const DEFAULT_RETENTION_DAYS: u64 = 30;

/// History trees inside a profile that hold rollout files.
const HISTORY_DIRS: [&str; 2] = ["sessions", "archived_sessions"];

/// The subset of `.loom/config.json → autonomous.codexSessionReaper` consumed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSessionReaperConfig {
    /// `autonomous.codexSessionReaper.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `autonomous.codexSessionReaper.retentionDays` (default 30, must be > 0).
    pub retention_days: Option<u64>,
}

/// Read the config block, soft-failing every field to `None`.
#[must_use]
pub fn read_config(repo_root: &Path) -> CodexSessionReaperConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.codexSessionReaper")
    else {
        return CodexSessionReaperConfig::default();
    };
    CodexSessionReaperConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        retention_days: block
            .get("retentionDays")
            .and_then(serde_json::Value::as_u64)
            .filter(|&d| d > 0),
    }
}

/// Resolve whether the pass runs — env > config > default(true).
#[must_use]
pub fn resolve_enabled(config: &CodexSessionReaperConfig) -> bool {
    if let Ok(v) = std::env::var(CODEX_SESSION_REAPER_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the retention floor in days — env > config > default(30).
#[must_use]
pub fn resolve_retention_days(config: &CodexSessionReaperConfig) -> u64 {
    std::env::var(CODEX_SESSION_RETENTION_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&d| d > 0)
        .or(config.retention_days)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CodexSessionReapOutcome {
    /// Rollout files deleted.
    pub files_removed: usize,
    /// Bytes those files held.
    pub bytes_reclaimed: u64,
    /// Emptied date directories removed.
    pub dirs_removed: usize,
    /// History dirs skipped because their real target is outside the allowed roots.
    pub skipped_outside_root: usize,
}

fn is_rollout(name: &str) -> bool {
    name.starts_with("rollout-")
        && Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
}

/// Prune `dir` (a real, non-symlink directory); returns true when it is left empty.
fn prune_tree(dir: &Path, cutoff: SystemTime, out: &mut CodexSessionReapOutcome) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut empty = true;
    for entry in entries.flatten() {
        let path = entry.path();
        // `DirEntry::file_type` does not follow symlinks: a symlink is neither
        // a dir nor a file here, so it is left alone and keeps `dir` non-empty.
        let Ok(ft) = entry.file_type() else {
            empty = false;
            continue;
        };
        if ft.is_dir() {
            if prune_tree(&path, cutoff, out) && std::fs::remove_dir(&path).is_ok() {
                out.dirs_removed += 1;
            } else if path.exists() {
                empty = false;
            }
        } else if ft.is_file() && is_rollout(&entry.file_name().to_string_lossy()) {
            let meta = entry.metadata().ok();
            let old = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .is_some_and(|t| t < cutoff);
            if old && std::fs::remove_file(&path).is_ok() {
                out.files_removed += 1;
                out.bytes_reclaimed += meta.map_or(0, |m| m.len());
            } else {
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    empty
}

/// Prune rollout history older than `cutoff` across every profile under
/// `profile_root`. `default_home` is the default Codex home that pooled
/// profiles symlink their history to; real targets must live under it or
/// under `profile_root`.
#[must_use]
pub fn prune_profiles(
    profile_root: &Path,
    default_home: Option<&Path>,
    cutoff: SystemTime,
) -> CodexSessionReapOutcome {
    let mut out = CodexSessionReapOutcome::default();
    let Ok(root) = profile_root.canonicalize() else {
        return out;
    };
    let mut allowed = vec![root.clone()];
    allowed.extend(default_home.and_then(|h| h.canonicalize().ok()));
    let Ok(profiles) = std::fs::read_dir(&root) else {
        return out;
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for profile in profiles.flatten() {
        let name = profile.file_name();
        let is_dir = profile.file_type().is_ok_and(|t| t.is_dir());
        if !is_dir || name.to_string_lossy().starts_with('.') {
            continue;
        }
        for history in HISTORY_DIRS {
            let Ok(real) = profile.path().join(history).canonicalize() else {
                continue;
            };
            if !real.is_dir() {
                continue;
            }
            if !allowed.iter().any(|a| real.starts_with(a)) {
                out.skipped_outside_root += 1;
                continue;
            }
            if seen.insert(real.clone()) {
                // The tree root itself is kept, even when emptied.
                prune_tree(&real, cutoff, &mut out);
            }
        }
    }
    out
}

fn default_codex_home() -> Option<PathBuf> {
    match std::env::var("CODEX_HOME") {
        Ok(v) if !v.trim().is_empty() => Some(PathBuf::from(v.trim())),
        _ => dirs::home_dir().map(|h| h.join(".codex")),
    }
}

/// Run one pass for the tick of `root`, gated by that repo's config. The
/// profile root is machine-level, so the pass is idempotent across roots.
pub async fn reap_codex_sessions_for(root: &Path) {
    let config = read_config(root);
    if !resolve_enabled(&config) {
        log::debug!("codex_session_reaper: disabled for {} — skipping", root.display());
        return;
    }
    let Some(profile_root) = crate::tokens_pool::paths::codex_profile_root() else {
        return;
    };
    let days = resolve_retention_days(&config);
    let cutoff = SystemTime::now() - Duration::from_secs(days * 86_400);
    let default_home = default_codex_home();
    let joined = tokio::task::spawn_blocking(move || {
        prune_profiles(&profile_root, default_home.as_deref(), cutoff)
    })
    .await;
    match joined {
        Ok(o) if o.files_removed > 0 || o.skipped_outside_root > 0 => log::info!(
            "codex_session_reaper: removed {} rollout(s) ({} bytes), {} empty dir(s); \
             skipped {} history dir(s) outside the profile root (retention {days}d)",
            o.files_removed,
            o.bytes_reclaimed,
            o.dirs_removed,
            o.skipped_outside_root
        ),
        Ok(_) => log::debug!("codex_session_reaper: nothing to prune"),
        Err(e) => log::error!("codex_session_reaper: pass panicked ({e}); continuing"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn age(path: &Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 86_400);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn rollout(dir: &Path, name: &str, days: u64) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, b"{}\n").unwrap();
        age(&p, days);
        p
    }

    fn cutoff() -> SystemTime {
        SystemTime::now() - Duration::from_secs(30 * 86_400)
    }

    #[test]
    fn prunes_old_keeps_new_and_protected_files() {
        let tmp = tempfile::tempdir().unwrap();
        let prof = tmp.path().join("alice");
        let day = prof.join("sessions/2026/01/02");
        let old = rollout(&day, "rollout-a.jsonl", 90);
        let new = rollout(&prof.join("sessions/2026/10/20"), "rollout-b.jsonl", 1);
        let mut protected = vec![];
        for f in [
            "auth.json",
            "config.toml",
            "hooks.json",
            ".loom-profile.json",
            ".session-managed.json",
            "thread_history_1.sqlite",
        ] {
            let p = prof.join(f);
            fs::write(&p, f).unwrap();
            age(&p, 365);
            protected.push((p, f));
        }
        let dot = prof.join(".tmp/sessions");
        let dotfile = rollout(&dot, "rollout-c.jsonl", 365);
        let other = rollout(&prof.join("sessions"), "notes.txt", 365);

        let out = prune_profiles(tmp.path(), None, cutoff());
        assert_eq!(out.files_removed, 1);
        assert!(!old.exists());
        assert!(!prof.join("sessions/2026/01").exists(), "emptied date dirs removed");
        assert!(new.exists() && dotfile.exists() && other.exists());
        for (p, f) in protected {
            assert_eq!(fs::read_to_string(p).unwrap(), f);
        }
        assert!(prof.join("sessions").is_dir(), "tree root kept");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_sessions_pruned_once_at_real_target() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("profiles");
        let default_home = tmp.path().join("dot-codex");
        let old = rollout(&default_home.join("sessions/2026/01/01"), "rollout-x.jsonl", 90);
        for p in ["alice", "bob"] {
            fs::create_dir_all(root.join(p)).unwrap();
            symlink(default_home.join("sessions"), root.join(p).join("sessions")).unwrap();
        }
        let out = prune_profiles(&root, Some(&default_home), cutoff());
        assert_eq!(out.files_removed, 1);
        assert!(!old.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_outside_allowed_roots_is_skipped() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("profiles");
        let elsewhere = tmp.path().join("elsewhere");
        let old = rollout(&elsewhere.join("sessions"), "rollout-x.jsonl", 90);
        fs::create_dir_all(root.join("alice")).unwrap();
        symlink(elsewhere.join("sessions"), root.join("alice/sessions")).unwrap();
        let out = prune_profiles(&root, None, cutoff());
        assert_eq!(out.files_removed, 0);
        assert_eq!(out.skipped_outside_root, 1);
        assert!(old.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_tree_is_not_followed() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("profiles");
        let outside = tmp.path().join("outside");
        let victim = rollout(&outside, "rollout-v.jsonl", 90);
        fs::create_dir_all(root.join("alice/sessions")).unwrap();
        symlink(&outside, root.join("alice/sessions/link")).unwrap();
        let _ = prune_profiles(&root, None, cutoff());
        assert!(victim.exists());
    }

    #[test]
    fn missing_root_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let out = prune_profiles(&tmp.path().join("nope"), None, cutoff());
        assert_eq!(out, CodexSessionReapOutcome::default());
    }

    #[test]
    fn retention_defaults() {
        let cfg = CodexSessionReaperConfig::default();
        if std::env::var(CODEX_SESSION_RETENTION_DAYS_ENV).is_err() {
            assert_eq!(resolve_retention_days(&cfg), DEFAULT_RETENTION_DAYS);
            let cfg = CodexSessionReaperConfig {
                enabled: None,
                retention_days: Some(7),
            };
            assert_eq!(resolve_retention_days(&cfg), 7);
        }
    }
}
