//! Observed per-repo agent-scope memory peaks and the RAM admission charge
//! derived from them (#11094, slice 1).
//!
//! # Why this exists
//!
//! [`crate::ram_headroom`] used to charge every sweep a flat 2 GB against a
//! point-in-time `MemAvailable` snapshot. A build-heavy repo can peak at 13 GB
//! per sweep, and a young sweep has not yet allocated its peak, so several
//! admissions each saw "plenty free" and the host OOM-killed the user session.
//!
//! This module closes both gaps without any new dependency:
//!
//! 1. **Record** each agent scope's `memory.peak` (cgroup v2). A transient
//!    scope's cgroup vanishes when its last process exits, so the peak cannot
//!    be read *after* the sweep ends; [`observe`] samples every live scope each
//!    work-finder tick (`memory.peak` is itself a monotonic high-water mark, so
//!    a sample never loses the peak), and when a scope disappears from the live
//!    set its last sample is folded into the repo's rolling history and
//!    emitted as telemetry.
//! 2. **Charge** admission with the repo's observed high-water mark
//!    ([`charge_gb`]) instead of the flat default, and **reserve** each
//!    in-flight sweep's not-yet-realised peak ([`reserved_bytes`]).
//!
//! A missing cgroup file (macOS, cgroup v1, a scope that never got created) is
//! a no-op, never an error. A repo with no history keeps the flat default and
//! reserves nothing, so behaviour without history is unchanged.
//!
//! # Every agent scope, not only sweeps
//!
//! The live set is the issue locks' stamped scopes **plus** every
//! `loom-agent-*.scope` found under [`AGENTS_SLICE`] ([`discover_scopes`]), so
//! a role agent (Doctor, Judge, …) that holds no issue lock is still sampled
//! and still reserved for. Such a scope is attributed to the workspace its
//! processes run in (their `/proc/<pid>/cwd`), else to
//! [`UNATTRIBUTED_REPO`], whose expected peak falls back to the host's worst
//! observed repo peak until it has a history of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Override for the store location (tests, odd hosts).
pub const PEAKS_PATH_ENV: &str = "LOOM_RAM_PEAKS_PATH";

/// Samples kept per repo. The charge is the max of these, so one outlier ages
/// out after this many later sweeps instead of pinning the repo forever.
pub const HISTORY_LEN: usize = 8;

/// Safety margin (percent) added on top of the observed high-water mark.
pub const MARGIN_PCT: u64 = 10;

const GIB: u64 = 1024 * 1024 * 1024;

/// History key for a live agent scope that maps to no managed workspace.
pub const UNATTRIBUTED_REPO: &str = "(unattributed)";

/// Unit-name prefix every `spawn-claude.sh` agent scope carries (#6129).
pub const AGENT_SCOPE_PREFIX: &str = "loom-agent-";

/// Prefix of `spawn-claude.sh`'s throwaway probe scopes, which are not agents.
const PROBE_SCOPE_PREFIX: &str = "loom-agent-probe-";

/// One live scope's last sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    pub repo: String,
    /// The claimed issue; `None` for a role agent with no issue lock.
    #[serde(default)]
    pub issue: Option<u32>,
    pub peak_bytes: u64,
    pub current_bytes: u64,
}

/// On-disk state: rolling peak history per repo plus the live scopes.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    /// Repo -> recent per-sweep peaks (bytes), oldest first.
    #[serde(default)]
    pub repos: BTreeMap<String, Vec<u64>>,
    /// Scope unit -> last sample of a still-running scope.
    #[serde(default)]
    pub inflight: BTreeMap<String, InFlight>,
}

/// A scope that was live at the previous observation and is gone now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedScope {
    pub scope: String,
    pub repo: String,
    pub issue: Option<u32>,
    pub peak_bytes: u64,
}

/// A live agent scope handed to [`observe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveScope {
    pub scope: String,
    pub repo: String,
    /// `None` for a scope found only by [`discover_scopes`] (a role agent).
    pub issue: Option<u32>,
}

/// Where a sweep's RAM charge came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeSource {
    Env,
    Observed,
    Default,
}

impl ChargeSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Observed => "observed",
            Self::Default => "default",
        }
    }
}

/// Parse a cgroup v2 single-value file (`memory.peak`, `memory.current`).
/// `max` and garbage are `None`.
#[must_use]
pub fn parse_cgroup_bytes(contents: &str) -> Option<u64> {
    contents.trim().parse().ok()
}

/// The repo's observed high-water mark (bytes), if it has history.
#[must_use]
pub fn high_water_bytes(samples: &[u64]) -> Option<u64> {
    samples.iter().copied().max().filter(|&b| b > 0)
}

/// Resolve the per-sweep charge: env override > observed high-water mark plus
/// [`MARGIN_PCT`] (rounded up to whole GB) > the flat default.
#[must_use]
pub fn charge_gb(
    env_gb: Option<u64>,
    observed_bytes: Option<u64>,
    default_gb: u64,
) -> (u64, ChargeSource) {
    if let Some(gb) = env_gb {
        return (gb.max(1), ChargeSource::Env);
    }
    if let Some(bytes) = observed_bytes {
        let padded = bytes.saturating_add(bytes / 100 * MARGIN_PCT);
        return (padded.div_ceil(GIB).max(1), ChargeSource::Observed);
    }
    (default_gb.max(1), ChargeSource::Default)
}

/// The peak a live scope of `repo` is expected to reach: its repo's
/// high-water mark. An [`UNATTRIBUTED_REPO`] scope with no history of its own
/// is assumed able to reach the host's worst observed repo peak, so a role
/// agent outside every workspace still counts toward the reservation.
#[must_use]
pub fn expected_peak_bytes(store: &Store, repo: &str) -> Option<u64> {
    store
        .repos
        .get(repo)
        .and_then(|h| high_water_bytes(h))
        .or_else(|| {
            (repo == UNATTRIBUTED_REPO)
                .then(|| {
                    store
                        .repos
                        .values()
                        .filter_map(|h| high_water_bytes(h))
                        .max()
                })
                .flatten()
        })
}

/// Bytes still to be realised by the live agent scopes (sweeps and role
/// agents alike): for each one with an expected peak
/// ([`expected_peak_bytes`]), `max(0, expected_peak - current_usage)`. A scope
/// that has already passed its expectation, or that has no expectation,
/// reserves nothing (so no-history behaviour is the pre-#11094 snapshot).
#[must_use]
pub fn reserved_bytes(store: &Store) -> u64 {
    store
        .inflight
        .values()
        .filter_map(|f| {
            let expected = expected_peak_bytes(store, &f.repo)?;
            Some(expected.saturating_sub(f.current_bytes))
        })
        .sum()
}

/// Per repo key, the in-flight SWEEPS (issue-lock scopes) whose future use
/// [`reserved_bytes`] accounts for: sampled into `inflight` AND with an
/// expected peak. Only these may be credited back into the admission cap; a
/// lock with no scope, an unreadable cgroup or a repo without history is
/// charged by admission instead (#11094).
#[must_use]
pub fn accounted_sweeps(store: &Store) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for f in store.inflight.values() {
        if f.issue.is_some() && expected_peak_bytes(store, &f.repo).is_some() {
            *out.entry(f.repo.clone()).or_insert(0) += 1;
        }
    }
    out
}

/// Fold one observation tick into `store`. `read` returns `(peak, current)`
/// bytes for a scope, or `None` when its cgroup files are unavailable (the
/// scope is then left out of `inflight`: a no-op, not an error). Scopes that
/// were tracked but are no longer live are folded into their repo's history
/// and returned.
pub fn observe(
    store: &mut Store,
    live: &[LiveScope],
    read: impl Fn(&str) -> Option<(u64, u64)>,
) -> Vec<EndedScope> {
    for l in live {
        let Some((peak, current)) = read(&l.scope) else {
            continue;
        };
        let prev_peak = store.inflight.get(&l.scope).map_or(0, |f| f.peak_bytes);
        store.inflight.insert(
            l.scope.clone(),
            InFlight {
                repo: l.repo.clone(),
                issue: l.issue,
                peak_bytes: peak.max(prev_peak),
                current_bytes: current,
            },
        );
    }
    let gone: Vec<String> = store
        .inflight
        .keys()
        .filter(|k| !live.iter().any(|l| &l.scope == *k))
        .cloned()
        .collect();
    let mut ended = Vec::new();
    for scope in gone {
        let Some(f) = store.inflight.remove(&scope) else {
            continue;
        };
        if f.peak_bytes > 0 {
            let hist = store.repos.entry(f.repo.clone()).or_default();
            hist.push(f.peak_bytes);
            if hist.len() > HISTORY_LEN {
                let excess = hist.len() - HISTORY_LEN;
                hist.drain(..excess);
            }
        }
        ended.push(EndedScope {
            scope,
            repo: f.repo,
            issue: f.issue,
            peak_bytes: f.peak_bytes,
        });
    }
    ended
}

/// Store location: `$LOOM_RAM_PEAKS_PATH`, else `$HOME/.loom/ram-peaks.json`.
#[must_use]
pub fn store_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(PEAKS_PATH_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".loom").join("ram-peaks.json"))
}

/// Load the store; a missing or corrupt file is an empty store.
#[must_use]
pub fn load(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Atomically persist the store (best effort).
pub fn save(path: &Path, store: &Store) {
    let Ok(json) = serde_json::to_string_pretty(store) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Repo key for a workspace root: its directory name.
#[must_use]
pub fn repo_key(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

/// The systemd slice `spawn-claude.sh` puts every agent scope in.
pub const AGENTS_SLICE: &str = "loom-agents.slice";

/// Relative cgroup path of a systemd slice unit. Dashes in a slice name encode
/// its parent chain (systemd.slice(5)), so `loom-agents.slice` lives at
/// `loom.slice/loom-agents.slice`, not directly under the user manager.
#[must_use]
pub fn slice_cgroup_path(slice: &str) -> Option<PathBuf> {
    let stem = slice.strip_suffix(".slice")?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(
        (0..parts.len())
            .map(|i| format!("{}.slice", parts[..=i].join("-")))
            .collect(),
    )
}

/// The cgroup v2 directory holding [`AGENTS_SLICE`] scopes for `uid`, under the
/// cgroup mount `cgroup_root`.
#[must_use]
pub fn agents_slice_dir_in(cgroup_root: &Path, uid: u32) -> Option<PathBuf> {
    Some(
        cgroup_root
            .join(format!("user.slice/user-{uid}.slice/user@{uid}.service"))
            .join(slice_cgroup_path(AGENTS_SLICE)?),
    )
}

#[cfg(target_os = "linux")]
fn agents_slice_dir() -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    agents_slice_dir_in(Path::new("/sys/fs/cgroup"), uid)
}

/// Read `(memory.peak, memory.current)` of `scope` under `slice_dir`.
#[must_use]
pub fn read_scope(slice_dir: &Path, scope: &str) -> Option<(u64, u64)> {
    if scope.contains('/') || scope.contains("..") {
        return None;
    }
    let dir = slice_dir.join(scope);
    let peak = parse_cgroup_bytes(&std::fs::read_to_string(dir.join("memory.peak")).ok()?)?;
    let current = std::fs::read_to_string(dir.join("memory.current"))
        .ok()
        .and_then(|s| parse_cgroup_bytes(&s))
        .unwrap_or(0);
    Some((peak, current))
}

/// Agent scope units directly under `slice_dir`: every `loom-agent-*.scope`
/// directory except `spawn-claude.sh`'s probe scopes, sorted.
#[must_use]
pub fn list_agent_scopes(slice_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(slice_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.starts_with(AGENT_SCOPE_PREFIX)
                && n.ends_with(".scope")
                && !n.starts_with(PROBE_SCOPE_PREFIX)
        })
        .collect();
    out.sort();
    out
}

/// The workspace a scope's processes run in: the first pid in its
/// `cgroup.procs` whose working directory (`cwd_of`) lies under one of
/// `roots` (the deepest root wins). `None` when no process maps to a root.
#[must_use]
pub fn scope_workspace(
    slice_dir: &Path,
    scope: &str,
    roots: &[PathBuf],
    cwd_of: impl Fn(u32) -> Option<PathBuf>,
) -> Option<String> {
    let procs = std::fs::read_to_string(slice_dir.join(scope).join("cgroup.procs")).ok()?;
    let canon: Vec<(usize, PathBuf)> = roots
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            std::iter::once((i, r.clone())).chain(r.canonicalize().ok().map(|c| (i, c)))
        })
        .collect();
    procs
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter_map(&cwd_of)
        .find_map(|cwd| {
            canon
                .iter()
                .filter(|(_, r)| cwd.starts_with(r))
                .max_by_key(|(_, r)| r.components().count())
                .map(|(i, _)| repo_key(&roots[*i]))
        })
}

/// The agent scopes under `slice_dir` that `known` (the issue-lock scopes)
/// does not already cover — role agents, chiefly — each attributed to its
/// workspace via [`scope_workspace`], else to [`UNATTRIBUTED_REPO`].
#[must_use]
pub fn discover_scopes(
    slice_dir: &Path,
    roots: &[PathBuf],
    known: &[LiveScope],
    cwd_of: impl Fn(u32) -> Option<PathBuf>,
) -> Vec<LiveScope> {
    list_agent_scopes(slice_dir)
        .into_iter()
        .filter(|s| !known.iter().any(|k| &k.scope == s))
        .map(|scope| {
            let repo = scope_workspace(slice_dir, &scope, roots, &cwd_of)
                .unwrap_or_else(|| UNATTRIBUTED_REPO.to_string());
            LiveScope {
                scope,
                repo,
                issue: None,
            }
        })
        .collect()
}

/// Emit the per-scope peak as telemetry (metric + log line). Only
/// [`record_tick`]'s Linux branch calls it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn emit_ended(e: &EndedScope) {
    let who = e
        .issue
        .map_or_else(|| "role agent".to_string(), |n| format!("issue #{n}"));
    log::info!(
        "ram_peaks: scope {} ({who}, repo {}) peaked at {} bytes ({:.2} GiB)",
        e.scope,
        e.repo,
        e.peak_bytes,
        e.peak_bytes as f64 / GIB as f64
    );
    use crate::telemetry::ops::{MetricName, MetricPoint};
    crate::observability::ops::emit_metrics(vec![MetricPoint::int(
        MetricName::AgentScopePeakMemoryBytes,
        i64::try_from(e.peak_bytes).unwrap_or(i64::MAX),
    )
    .label("repo", e.repo.clone())]);
}

/// The live agent scopes of every managed root — each registry's claim locks'
/// stamped scope units, plus every other `loom-agent-*.scope` under the agents
/// slice ([`discover_scopes`], Linux only), so role agents without an issue
/// lock are sampled and reserved too — and the number of in-flight sweeps
/// (issue locks) of each root, parallel to `roots`. Every lock is counted,
/// sampled or not, so admission can charge the ones no sample accounts for.
#[must_use]
pub fn live_scopes(
    pool: &std::sync::Arc<crate::workspace_pool::WorkspacePool>,
    roots: &[PathBuf],
) -> (Vec<LiveScope>, Vec<usize>) {
    let mut out = Vec::new();
    let mut in_flight = vec![0; roots.len()];
    for (idx, root) in roots.iter().enumerate() {
        let registry = pool.get_or_provision(root);
        let sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for a in sr.in_flight_snapshot() {
            in_flight[idx] += 1;
            if let Some(scope) = a.scope_unit {
                out.push(LiveScope {
                    scope,
                    repo: repo_key(root),
                    issue: Some(a.issue),
                });
            }
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(slice) = agents_slice_dir() {
        let cwd_of = |pid: u32| std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let extra = discover_scopes(&slice, roots, &out, cwd_of);
        out.extend(extra);
    }
    (out, in_flight)
}

/// One observation pass over the live scopes of all managed roots. Reads the
/// cgroup files, updates and persists the store, emits telemetry for ended
/// scopes. Linux only; elsewhere a no-op.
pub fn record_tick(live: &[LiveScope]) {
    #[cfg(target_os = "linux")]
    {
        let (Some(path), Some(slice)) = (store_path(), agents_slice_dir()) else {
            return;
        };
        let mut store = load(&path);
        let before = store.clone();
        let ended = observe(&mut store, live, |s| read_scope(&slice, s));
        for e in &ended {
            emit_ended(e);
        }
        if store != before {
            save(&path, &store);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = live;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn live(scope: &str, repo: &str) -> LiveScope {
        LiveScope {
            scope: scope.into(),
            repo: repo.into(),
            issue: Some(1),
        }
    }

    #[test]
    fn parse_cgroup_bytes_handles_max_and_garbage() {
        assert_eq!(parse_cgroup_bytes("12345\n"), Some(12345));
        assert_eq!(parse_cgroup_bytes("max\n"), None);
        assert_eq!(parse_cgroup_bytes(""), None);
    }

    #[test]
    fn charge_precedence_env_observed_default() {
        assert_eq!(charge_gb(Some(4), Some(13 * GIB), 2), (4, ChargeSource::Env));
        // 13 GiB + 10% = 14.3 -> 15
        assert_eq!(charge_gb(None, Some(13 * GIB), 2), (15, ChargeSource::Observed));
        assert_eq!(charge_gb(None, None, 2), (2, ChargeSource::Default));
    }

    #[test]
    fn observe_records_peak_when_scope_ends_and_keeps_rolling_window() {
        let mut store = Store::default();
        let ended = observe(&mut store, &[live("a.scope", "r")], |_| Some((5 * GIB, GIB)));
        assert!(ended.is_empty());
        // Peak is monotonic even if a later sample is lower.
        observe(&mut store, &[live("a.scope", "r")], |_| Some((3 * GIB, 2 * GIB)));
        assert_eq!(store.inflight["a.scope"].peak_bytes, 5 * GIB);
        let ended = observe(&mut store, &[], |_| None);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].peak_bytes, 5 * GIB);
        assert_eq!(store.repos["r"], vec![5 * GIB]);
        assert!(store.inflight.is_empty());

        for i in 0..(HISTORY_LEN as u64 + 3) {
            observe(&mut store, &[live("b.scope", "r")], |_| Some((i + 1, 0)));
            observe(&mut store, &[], |_| None);
        }
        assert_eq!(store.repos["r"].len(), HISTORY_LEN);
    }

    #[test]
    fn missing_cgroup_is_a_noop() {
        let mut store = Store::default();
        let ended = observe(&mut store, &[live("a.scope", "r")], |_| None);
        assert!(ended.is_empty());
        assert!(store.inflight.is_empty());
        assert!(store.repos.is_empty());
        assert_eq!(read_scope(Path::new("/nonexistent-loom-test"), "x.scope"), None);
    }

    #[test]
    fn slice_cgroup_path_expands_dash_hierarchy() {
        assert_eq!(
            slice_cgroup_path("loom-agents.slice"),
            Some(PathBuf::from("loom.slice/loom-agents.slice"))
        );
        assert_eq!(
            slice_cgroup_path("a-b-c.slice"),
            Some(PathBuf::from("a.slice/a-b.slice/a-b-c.slice"))
        );
        assert_eq!(slice_cgroup_path("plain.slice"), Some(PathBuf::from("plain.slice")));
        assert_eq!(slice_cgroup_path("x.scope"), None);
        assert_eq!(slice_cgroup_path("a--b.slice"), None);
    }

    #[test]
    fn production_path_reads_a_scope_under_the_nested_slice() {
        // Mirror the real cgroup tree that `systemd-run --user --scope
        // --slice=loom-agents.slice` produces and resolve it the way
        // `record_tick` does (via `agents_slice_dir_in`, not a hand-made dir).
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join(
            "user.slice/user-1000.slice/user@1000.service/loom.slice/loom-agents.slice/loom-agent-1.scope",
        );
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::write(scope.join("memory.peak"), "9000\n").unwrap();
        std::fs::write(scope.join("memory.current"), "4000\n").unwrap();
        let dir = agents_slice_dir_in(root.path(), 1000).unwrap();
        assert_eq!(read_scope(&dir, "loom-agent-1.scope"), Some((9000, 4000)));
        // The pre-fix flat path (no `loom.slice` parent) must not resolve.
        let flat = root
            .path()
            .join("user.slice/user-1000.slice/user@1000.service/loom-agents.slice");
        assert_eq!(read_scope(&flat, "loom-agent-1.scope"), None);
    }

    #[test]
    fn read_scope_reads_files_and_rejects_traversal() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("loom-agent-1.scope");
        std::fs::create_dir(&s).unwrap();
        std::fs::write(s.join("memory.peak"), "9000\n").unwrap();
        std::fs::write(s.join("memory.current"), "4000\n").unwrap();
        assert_eq!(read_scope(d.path(), "loom-agent-1.scope"), Some((9000, 4000)));
        assert_eq!(read_scope(d.path(), "../x"), None);
    }

    #[test]
    fn reservation_counts_only_repos_with_history_and_saturates() {
        let mut store = Store::default();
        store.repos.insert("hist".into(), vec![13 * GIB]);
        let f = |repo: &str, cur: u64| InFlight {
            repo: repo.into(),
            issue: Some(1),
            peak_bytes: cur,
            current_bytes: cur,
        };
        store.inflight.insert("a".into(), f("hist", 4 * GIB));
        store.inflight.insert("b".into(), f("nohist", GIB));
        store.inflight.insert("c".into(), f("hist", 20 * GIB)); // past expectation
        assert_eq!(reserved_bytes(&store), 9 * GIB);
    }

    /// A fake agents slice: `(scope, peak, current, pids)` per scope dir.
    fn fake_slice(scopes: &[(&str, u64, u64, &[u32])]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, peak, cur, pids) in scopes {
            let s = d.path().join(name);
            std::fs::create_dir(&s).unwrap();
            std::fs::write(s.join("memory.peak"), format!("{peak}\n")).unwrap();
            std::fs::write(s.join("memory.current"), format!("{cur}\n")).unwrap();
            let procs: String = pids.iter().map(|p| format!("{p}\n")).collect();
            std::fs::write(s.join("cgroup.procs"), procs).unwrap();
        }
        d
    }

    #[test]
    fn discovery_finds_role_scopes_without_an_issue_lock() {
        // One sweep scope known from its issue lock, one Doctor scope running
        // in a `small` worktree, one agent outside every workspace, and a
        // spawn-claude probe scope that is not an agent.
        let slice = fake_slice(&[
            ("loom-agent-10-1.scope", GIB, GIB, &[10]),
            ("loom-agent-20-2.scope", 7 * GIB, 3 * GIB, &[20, 21]),
            ("loom-agent-30-3.scope", GIB, GIB, &[30]),
            ("loom-agent-probe-40-4.scope", 0, 0, &[]),
        ]);
        std::fs::write(slice.path().join("not-a-dir.scope"), "").unwrap();
        let roots = vec![PathBuf::from("/w/heavy"), PathBuf::from("/w/small")];
        let cwd = |pid: u32| match pid {
            20 => None, // exited between the procs read and the readlink
            21 => Some(PathBuf::from("/w/small/.loom/worktrees/pr-7")),
            30 => Some(PathBuf::from("/tmp/elsewhere")),
            _ => None,
        };
        let known = vec![LiveScope {
            scope: "loom-agent-10-1.scope".into(),
            repo: "heavy".into(),
            issue: Some(5),
        }];
        let found = discover_scopes(slice.path(), &roots, &known, cwd);
        assert_eq!(
            found,
            vec![
                LiveScope {
                    scope: "loom-agent-20-2.scope".into(),
                    repo: "small".into(),
                    issue: None,
                },
                LiveScope {
                    scope: "loom-agent-30-3.scope".into(),
                    repo: UNATTRIBUTED_REPO.into(),
                    issue: None,
                },
            ]
        );

        // Sampling: the role scopes are observed like a sweep, and a finished
        // Doctor's peak lands in its workspace's history.
        let mut store = Store::default();
        let live: Vec<LiveScope> = known.into_iter().chain(found).collect();
        observe(&mut store, &live, |s| read_scope(slice.path(), s));
        assert_eq!(store.inflight["loom-agent-20-2.scope"].issue, None);
        assert_eq!(store.inflight["loom-agent-20-2.scope"].peak_bytes, 7 * GIB);
        let ended = observe(&mut store, &live[..1], |s| read_scope(slice.path(), s));
        assert_eq!(ended.len(), 2);
        assert_eq!(store.repos["small"], vec![7 * GIB]);
        assert_eq!(store.repos[UNATTRIBUTED_REPO], vec![GIB]);
    }

    #[test]
    fn role_scopes_without_an_issue_lock_are_reserved_for() {
        let mut store = Store::default();
        store.repos.insert("heavy".into(), vec![13 * GIB]);
        store.repos.insert("small".into(), vec![3 * GIB]);
        let role = |repo: &str, cur: u64| InFlight {
            repo: repo.into(),
            issue: None,
            peak_bytes: cur,
            current_bytes: cur,
        };
        // A Doctor in `small` at 1 GiB of its 3 GiB history reserves 2 GiB.
        store.inflight.insert("doctor".into(), role("small", GIB));
        assert_eq!(reserved_bytes(&store), 2 * GIB);
        // An unattributed agent with no history of its own is assumed able to
        // reach the host's worst repo peak (13 GiB): 13 - 4 = 9 more.
        store
            .inflight
            .insert("judge".into(), role(UNATTRIBUTED_REPO, 4 * GIB));
        assert_eq!(reserved_bytes(&store), 11 * GIB);
        // Once unattributed scopes have their own history, it wins.
        store.repos.insert(UNATTRIBUTED_REPO.into(), vec![5 * GIB]);
        assert_eq!(reserved_bytes(&store), 3 * GIB);
    }

    #[test]
    fn stored_issue_numbers_from_the_previous_schema_still_load() {
        let raw = r#"{"inflight":{"s":{"repo":"r","issue":5,"peak_bytes":1,"current_bytes":1}}}"#;
        let s: Store = serde_json::from_str(raw).unwrap();
        assert_eq!(s.inflight["s"].issue, Some(5));
    }

    #[test]
    fn store_round_trips_and_corrupt_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("p.json");
        let mut s = Store::default();
        s.repos.insert("r".into(), vec![1, 2]);
        save(&p, &s);
        assert_eq!(load(&p), s);
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load(&p), Store::default());
    }
}
