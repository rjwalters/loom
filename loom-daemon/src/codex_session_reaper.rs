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
//! **Opt-in** (operator ruling 2026-11-16): a deleted rollout cannot be
//! restored by a revert, so nothing is deleted until a host turns the pass on.
//! While it is off, the pass only *measures* — at most once per reaper tick it
//! logs how many rollout files are older than the floor and how many bytes
//! they hold, so an operator can see what enabling it would remove.
//!
//! Config precedence **env > config > default**, and the config tier is
//! **host-scoped only**: the private defaults file, the per-repo fleet tier
//! (`.loom-local/fleet-repo.json`) and the host-local override
//! (`.loom-local/local.json`), never the repo-committed
//! `.loom/config.json` / `.loom-project/project.json` — the pass is
//! machine-level (one profile root shared by every registered repo), so a
//! repo-committed key must not switch host-wide deletion on. Keys:
//! `LOOM_CODEX_SESSION_REAPER` / `autonomous.codexSessionReaper.enabled`
//! (default **off**) and `LOOM_CODEX_SESSION_RETENTION_DAYS` /
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

/// The subset of the host-scoped `autonomous.codexSessionReaper` block (see
/// [`read_config`]) consumed here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSessionReaperConfig {
    /// `autonomous.codexSessionReaper.enabled` (default **false**: opt-in).
    pub enabled: Option<bool>,
    /// `autonomous.codexSessionReaper.retentionDays` (default 30, must be > 0).
    pub retention_days: Option<u64>,
}

/// Read the config block from **host-scoped tiers only**: the private
/// defaults file, the per-repo fleet tier (`.loom-local/fleet-repo.json`)
/// and the host-local override (`.loom-local/local.json`) — never the
/// repo-committed `.loom/config.json` / `.loom-project/project.json`. The
/// pass is machine-level, so a repo-committed key must not turn host-wide
/// deletion on. Soft-fails every field to `None`.
#[must_use]
pub fn read_config(repo_root: &Path) -> CodexSessionReaperConfig {
    let effective = read_host_scoped_config(repo_root);
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

/// The host-scoped half of
/// [`config_resolver::resolve_effective_config`]: private defaults <
/// per-repo fleet tier < host-local override, with the repo-committed tiers
/// (`.loom/config.json`, `.loom-project/project.json`) deliberately absent —
/// the delete decision this feeds is machine-level, and a tracked repo
/// config must not make it for every host that registers the repo.
#[must_use]
fn read_host_scoped_config(repo_root: &Path) -> serde_json::Value {
    use crate::config_resolver::{
        deep_merge, private_defaults_path, soft_read_json_object, FLEET_REPO_CONFIG_REL,
        LOCAL_CONFIG_REL,
    };
    let mut effective = serde_json::Value::Object(serde_json::Map::new());
    if let Some(defaults) = private_defaults_path() {
        effective = deep_merge(&effective, &soft_read_json_object(&defaults));
    }
    effective =
        deep_merge(&effective, &soft_read_json_object(&repo_root.join(FLEET_REPO_CONFIG_REL)));
    effective = deep_merge(&effective, &soft_read_json_object(&repo_root.join(LOCAL_CONFIG_REL)));
    effective
}

/// Resolve whether the pass deletes — env > config > default(**false**).
#[must_use]
pub fn resolve_enabled(config: &CodexSessionReaperConfig) -> bool {
    let env = std::env::var(CODEX_SESSION_REAPER_ENABLE_ENV).ok();
    resolve_enabled_from(env.as_deref(), config)
}

/// [`resolve_enabled`] with the env value passed in. A set env var wins and
/// enables only on an explicit truthy value; unset falls to the config key,
/// and an unset config key means **off**.
#[must_use]
pub fn resolve_enabled_from(env: Option<&str>, config: &CodexSessionReaperConfig) -> bool {
    if let Some(v) = env {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
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
    /// Rollout files older than the floor that were found and left in place
    /// (a measure-only pass, or a delete that failed).
    pub files_eligible: usize,
    /// Bytes those left-in-place files hold.
    pub bytes_eligible: u64,
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

/// Walk `dir` (a real, non-symlink directory); returns true when it is left
/// empty. With `delete` false nothing on disk is changed: old rollouts are
/// only counted into `files_eligible` / `bytes_eligible`.
fn prune_tree(
    dir: &Path,
    cutoff: SystemTime,
    delete: bool,
    out: &mut CodexSessionReapOutcome,
) -> bool {
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
            if prune_tree(&path, cutoff, delete, out)
                && delete
                && std::fs::remove_dir(&path).is_ok()
            {
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
            let len = meta.map_or(0, |m| m.len());
            if old && delete && std::fs::remove_file(&path).is_ok() {
                out.files_removed += 1;
                out.bytes_reclaimed += len;
            } else {
                if old {
                    out.files_eligible += 1;
                    out.bytes_eligible += len;
                }
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
    scan_profiles(profile_root, default_home, cutoff, true)
}

/// Measure-only twin of [`prune_profiles`]: the same walk, the same symlink
/// and allowed-roots rules, but nothing is deleted. The rollouts a real pass
/// would remove are reported in `files_eligible` / `bytes_eligible`.
#[must_use]
pub fn measure_profiles(
    profile_root: &Path,
    default_home: Option<&Path>,
    cutoff: SystemTime,
) -> CodexSessionReapOutcome {
    scan_profiles(profile_root, default_home, cutoff, false)
}

fn scan_profiles(
    profile_root: &Path,
    default_home: Option<&Path>,
    cutoff: SystemTime,
    delete: bool,
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
                prune_tree(&real, cutoff, delete, &mut out);
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

/// Cutoff `days` in the past, or `None` when that cannot be represented:
/// `days * 86_400` wraps for values from ~2.1e14 (release builds carry no
/// overflow checks, wrapping the cutoff to just before **now**, which would
/// delete recent rollouts), and from ~1.1e14 the `SystemTime` subtraction
/// itself panics. A floor that overflows fails closed — the caller skips
/// the pass.
fn cutoff_from_days(now: SystemTime, days: u64) -> Option<SystemTime> {
    let secs = days.checked_mul(86_400)?;
    now.checked_sub(Duration::from_secs(secs))
}

/// Run one pass for the tick of `root`, gated by that repo's config. The
/// profile root is machine-level, so the pass is idempotent across roots.
///
/// Enabled: delete rollouts older than the floor. Disabled (the default):
/// delete nothing, and report what enabling it would remove. `reported` is the
/// caller's per-tick flag: the disabled report is machine-level too, so it is
/// made at most once per tick however many registered roots there are.
pub async fn reap_codex_sessions_for(root: &Path, reported: &mut bool) {
    let config = read_config(root);
    let enabled = resolve_enabled(&config);
    if !enabled && *reported {
        return;
    }
    let Some(profile_root) = crate::tokens_pool::paths::codex_profile_root() else {
        return;
    };
    let days = resolve_retention_days(&config);
    let Some(cutoff) = cutoff_from_days(SystemTime::now(), days) else {
        log::warn!(
            "codex_session_reaper: retention {days}d overflows the age floor; skipping this \
             pass — fix {CODEX_SESSION_RETENTION_DAYS_ENV} / \
             autonomous.codexSessionReaper.retentionDays"
        );
        return;
    };
    let default_home = default_codex_home();
    let joined = tokio::task::spawn_blocking(move || {
        scan_profiles(&profile_root, default_home.as_deref(), cutoff, enabled)
    })
    .await;
    // A tick that pruned or measured has nothing more to report.
    *reported = true;
    match joined {
        Ok(o) if enabled && (o.files_removed > 0 || o.skipped_outside_root > 0) => log::info!(
            "codex_session_reaper: removed {} rollout(s) ({} bytes), {} empty dir(s); \
             skipped {} history dir(s) outside the profile root (retention {days}d)",
            o.files_removed,
            o.bytes_reclaimed,
            o.dirs_removed,
            o.skipped_outside_root
        ),
        Ok(_) if enabled => log::debug!("codex_session_reaper: nothing to prune"),
        Ok(o) if o.files_eligible > 0 => log::info!(
            "codex_session_reaper: disabled, nothing deleted; enabling it would remove {} \
             rollout(s) ({} bytes) older than {days}d ({} history dir(s) outside the profile \
             root would be skipped). Opt in with {CODEX_SESSION_REAPER_ENABLE_ENV}=1 or \
             autonomous.codexSessionReaper.enabled=true in a host-scoped tier \
             (.loom-local/local.json, .loom-local/fleet-repo.json or the private defaults)",
            o.files_eligible,
            o.bytes_eligible,
            o.skipped_outside_root
        ),
        Ok(_) => log::debug!(
            "codex_session_reaper: disabled, nothing deleted; no rollout older than {days}d"
        ),
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
    fn disabled_by_default_and_enabled_only_explicitly() {
        let unset = CodexSessionReaperConfig::default();
        assert!(!resolve_enabled_from(None, &unset), "unset env + unset config is off");
        let on = CodexSessionReaperConfig {
            enabled: Some(true),
            retention_days: None,
        };
        assert!(resolve_enabled_from(None, &on));
        for v in ["1", "true", "YES", " on "] {
            assert!(resolve_enabled_from(Some(v), &unset), "{v}");
        }
        // A set env var wins over the config key, and only a truthy value enables.
        for v in ["0", "false", "", "maybe"] {
            assert!(!resolve_enabled_from(Some(v), &on), "{v:?}");
        }
    }

    /// Every path under `dir`, sorted, with file sizes — a before/after
    /// fingerprint proving a pass changed nothing.
    fn listing(dir: &Path) -> Vec<(PathBuf, u64)> {
        let mut out = vec![];
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                let ft = e.file_type().unwrap();
                if ft.is_dir() {
                    stack.push(p.clone());
                }
                out.push((
                    p,
                    if ft.is_file() {
                        e.metadata().unwrap().len()
                    } else {
                        0
                    },
                ));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn default_configuration_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let prof = tmp.path().join("alice");
        rollout(&prof.join("sessions/2026/01/02"), "rollout-a.jsonl", 90);
        rollout(&prof.join("archived_sessions/2025/12/31"), "rollout-b.jsonl", 400);
        let before = listing(tmp.path());

        // The pass exactly as `reap_codex_sessions_for` runs it with nothing
        // configured: no env var, no config key.
        let delete = resolve_enabled_from(None, &CodexSessionReaperConfig::default());
        let out = scan_profiles(tmp.path(), None, cutoff(), delete);

        assert_eq!(listing(tmp.path()), before, "nothing on disk changed");
        assert_eq!((out.files_removed, out.bytes_reclaimed, out.dirs_removed), (0, 0, 0));
    }

    #[test]
    fn disabled_pass_reports_and_leaves_every_file_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let prof = tmp.path().join("alice");
        let old_a = rollout(&prof.join("sessions/2026/01/02"), "rollout-a.jsonl", 90);
        let old_b = rollout(&prof.join("archived_sessions/2025/12/31"), "rollout-b.jsonl", 400);
        fs::write(&old_b, b"0123456789").unwrap();
        age(&old_b, 400);
        rollout(&prof.join("sessions/2026/10/20"), "rollout-new.jsonl", 1);
        rollout(&prof.join("sessions"), "notes.txt", 365);
        let before = listing(tmp.path());

        let out = measure_profiles(tmp.path(), None, cutoff());

        assert_eq!(listing(tmp.path()), before, "nothing on disk changed");
        assert_eq!((out.files_removed, out.bytes_reclaimed, out.dirs_removed), (0, 0, 0));
        // Only the two old rollouts are reported: not the new one, not notes.txt.
        assert_eq!(out.files_eligible, 2);
        let expect = fs::metadata(&old_a).unwrap().len() + fs::metadata(&old_b).unwrap().len();
        assert_eq!(out.bytes_eligible, expect);

        // The report matches what enabling it then removes.
        let pruned = prune_profiles(tmp.path(), None, cutoff());
        assert_eq!(pruned.files_removed, out.files_eligible);
        assert_eq!(pruned.bytes_reclaimed, out.bytes_eligible);
        assert_eq!(pruned.files_eligible, 0);
        assert!(!old_a.exists() && !old_b.exists());
    }

    #[cfg(unix)]
    #[test]
    fn disabled_pass_counts_a_shared_symlink_target_once_and_skips_outside_roots() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("profiles");
        let default_home = tmp.path().join("dot-codex");
        let elsewhere = tmp.path().join("elsewhere");
        let shared = rollout(&default_home.join("sessions/2026/01/01"), "rollout-x.jsonl", 90);
        let outside = rollout(&elsewhere.join("sessions"), "rollout-y.jsonl", 90);
        for p in ["alice", "bob"] {
            fs::create_dir_all(root.join(p)).unwrap();
            symlink(default_home.join("sessions"), root.join(p).join("sessions")).unwrap();
        }
        fs::create_dir_all(root.join("carol")).unwrap();
        symlink(elsewhere.join("sessions"), root.join("carol/sessions")).unwrap();

        let out = measure_profiles(&root, Some(&default_home), cutoff());

        assert_eq!(out.files_eligible, 1, "shared target counted once, outside root not at all");
        assert_eq!(out.skipped_outside_root, 1);
        assert!(shared.exists() && outside.exists());
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

    /// A retention floor that cannot be represented fails closed: no cutoff,
    /// so the pass is skipped instead of wrapping to "delete everything
    /// recent" (release builds have no overflow checks).
    #[test]
    fn retention_overflow_fails_closed() {
        let now = SystemTime::now();
        assert_eq!(cutoff_from_days(now, 30), now.checked_sub(Duration::from_secs(30 * 86_400)));
        // `days * 86_400` overflows u64.
        assert_eq!(cutoff_from_days(now, u64::MAX), None);
        // The product fits u64 but lands before the UNIX epoch.
        assert_eq!(cutoff_from_days(now, 10_u64.pow(15)), None);
    }

    /// Repo-committed tiers must not steer a machine-level delete: only the
    /// host-scoped tiers (private defaults, fleet tier, `.loom-local`) are
    /// consulted for this block.
    #[test]
    fn repo_committed_config_cannot_enable_host_wide_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        for rel in [".loom/config.json", ".loom-project/project.json"] {
            let p = tmp.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(
                &p,
                serde_json::json!({"autonomous": {"codexSessionReaper": {
                    "enabled": true, "retentionDays": 1,
                }}})
                .to_string(),
            )
            .unwrap();
        }
        assert_eq!(
            read_config(tmp.path()),
            CodexSessionReaperConfig::default(),
            "repo-committed tiers are not consulted"
        );

        let fleet = tmp.path().join(".loom-local/fleet-repo.json");
        fs::create_dir_all(fleet.parent().unwrap()).unwrap();
        fs::write(
            &fleet,
            serde_json::json!({"autonomous": {"codexSessionReaper": {"retentionDays": 7}}})
                .to_string(),
        )
        .unwrap();
        let local = tmp.path().join(".loom-local/local.json");
        fs::write(
            &local,
            serde_json::json!({"autonomous": {"codexSessionReaper": {"enabled": true}}})
                .to_string(),
        )
        .unwrap();
        let cfg = read_config(tmp.path());
        assert_eq!(cfg.enabled, Some(true), "host-local override enables");
        assert_eq!(cfg.retention_days, Some(7), "fleet tier supplies retention");
        assert!(resolve_enabled_from(None, &cfg));
    }
}
