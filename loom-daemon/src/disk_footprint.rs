//! Observed per-repo disk footprint of sweeps, and the in-flight units disk
//! admission reserves growth for (#11191). The disk twin of
//! [`crate::ram_peaks`].
//!
//! # Why this exists
//!
//! The work finder's disk term was `floor(free_gb / LOOM_PER_WORKTREE_GB)`: a
//! flat 8 GB per sweep, measured against free space *now*. A loom sweep's run
//! dir reaches about 26 GB, and a sweep admitted before its compile step has
//! written almost none of it, so free space looked fine while two in-flight
//! builds were still going to write 50 GB (2026-10-09, loom-worker-1).
//!
//! This module supplies the two missing numbers:
//!
//! 1. **A per-repo high-water mark.** A sampler thread measures every live
//!    sweep's footprint each [`DEFAULT_SAMPLE_SECS`]. When a unit leaves the
//!    live set its peak is folded into its repo's rolling history (the last
//!    [`HISTORY_LEN`] runs). [`crate::disk_admission`] charges the max of that.
//! 2. **The in-flight units**, with what each has written so far, so admission
//!    can reserve what each is still expected to write.
//!
//! # What a unit is
//!
//! - An **issue sweep** (a live claim lock under `<repo>/.loom/locks/issue-N`):
//!   its worktree (`<worktree-root>/issue-N`) plus every live Loom run dir
//!   (`<repo>/.loom/targets/<role>-<run>`, #8370) whose owner pid is the
//!   lock's pid or in the lock's process group. Key `<repo>#<N>`.
//! - Any **other live run dir** (a role-runner tick, a PR-set sweep): the dir
//!   alone. Key `<repo>:<dir name>`.
//!
//! Admission also records a *pending* unit the moment it admits a dispatch
//! ([`record_pending`]), so a sweep that no sample has seen yet is reserved
//! for. A pending unit lives [`PENDING_GRACE_SECS`] unless a sample adopts it.
//!
//! Every probe is best-effort: an unreadable lock or dir is skipped, never an
//! error. A repo with no history keeps its configured or default charge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

/// Override for the store location (tests, odd hosts).
pub const STORE_PATH_ENV: &str = "LOOM_DISK_FOOTPRINT_PATH";

/// Override for the sampler interval, in seconds.
pub const SAMPLE_SECS_ENV: &str = "LOOM_DISK_SAMPLE_SECS";

/// Default sampler interval: one work-finder tick.
pub const DEFAULT_SAMPLE_SECS: u64 = 60;

/// Runs kept per repo. The charge is their max, so it takes this many later
/// runs for one heavy build to age out. Long on purpose: a run of no-op sweeps
/// must not talk a build-heavy repo's charge down before its next build.
pub const HISTORY_LEN: usize = 20;

/// How long a pending unit (admitted, not yet sampled) is kept without a
/// sample adopting it.
pub const PENDING_GRACE_SECS: i64 = 180;

/// A store not sampled for this long is stale: its sampled units are ignored
/// (fail open), so a stopped sampler cannot pin a reservation forever.
pub const STALE_AFTER_SECS: i64 = 600;

/// Bytes per GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// One in-flight unit's last sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    pub repo: String,
    /// The claimed issue, for an issue sweep.
    #[serde(default)]
    pub issue: Option<u32>,
    /// Bytes on disk now.
    pub current_bytes: u64,
    /// Largest sample so far.
    pub peak_bytes: u64,
    /// Unix seconds at which admission recorded this unit, while no sample
    /// has seen it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_since: Option<i64>,
}

impl Unit {
    /// Whether this unit counts toward a reservation at `now`: a fresh pending
    /// unit always does; a sampled one only while the store is not stale.
    #[must_use]
    pub fn counts(&self, store_fresh: bool, now: i64) -> bool {
        match self.pending_since {
            Some(since) => now.saturating_sub(since) < PENDING_GRACE_SECS,
            None => store_fresh,
        }
    }

    /// Whether this unit is a sweep (issue or PR set) rather than a role run.
    #[must_use]
    pub fn is_sweep(&self, key: &str) -> bool {
        self.issue.is_some() || key.contains('#')
    }
}

/// On-disk state: per-repo footprint history plus the in-flight units.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    /// Repo -> recent per-run peak footprints (bytes), oldest first.
    #[serde(default)]
    pub repos: BTreeMap<String, Vec<u64>>,
    /// Unit key -> last sample.
    #[serde(default)]
    pub inflight: BTreeMap<String, Unit>,
    /// Unix seconds of the last completed sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at: Option<i64>,
}

impl Store {
    /// Whether the last sample is recent enough to trust its units.
    #[must_use]
    pub fn fresh(&self, now: i64) -> bool {
        self.sampled_at
            .is_some_and(|t| now.saturating_sub(t) < STALE_AFTER_SECS)
    }

    /// `repo`'s observed high-water mark (bytes), if it has history.
    #[must_use]
    pub fn high_water_bytes(&self, repo: &str) -> Option<u64> {
        self.repos
            .get(repo)
            .and_then(|h| h.iter().copied().max())
            .filter(|&b| b > 0)
    }
}

/// A unit seen live by one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveUnit {
    pub key: String,
    pub repo: String,
    pub issue: Option<u32>,
    pub bytes: u64,
}

/// A unit that was live and is gone now, with the peak folded into history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub key: String,
    pub repo: String,
    pub peak_bytes: u64,
}

/// The key of an issue sweep's unit.
#[must_use]
pub fn issue_key(repo: &str, issue: u32) -> String {
    format!("{repo}#{issue}")
}

/// Fold one sample into `store`. Live units are upserted (peak is monotonic,
/// and a sample adopts a pending unit of the same key). A tracked unit that
/// is not live is dropped: a pending one only once its grace has run out, and
/// without touching history; a sampled one has its peak folded into its
/// repo's history and is returned.
pub fn observe(store: &mut Store, live: &[LiveUnit], now: i64) -> Vec<Ended> {
    for l in live {
        let prev_peak = store.inflight.get(&l.key).map_or(0, |u| u.peak_bytes);
        store.inflight.insert(
            l.key.clone(),
            Unit {
                repo: l.repo.clone(),
                issue: l.issue,
                current_bytes: l.bytes,
                peak_bytes: l.bytes.max(prev_peak),
                pending_since: None,
            },
        );
    }
    let gone: Vec<String> = store
        .inflight
        .iter()
        .filter(|(k, _)| !live.iter().any(|l| &l.key == *k))
        .filter(|(_, u)| {
            !u.pending_since
                .is_some_and(|s| now.saturating_sub(s) < PENDING_GRACE_SECS)
        })
        .map(|(k, _)| k.clone())
        .collect();
    let mut ended = Vec::new();
    for key in gone {
        let Some(u) = store.inflight.remove(&key) else {
            continue;
        };
        if u.pending_since.is_some() || u.peak_bytes == 0 {
            continue;
        }
        let hist = store.repos.entry(u.repo.clone()).or_default();
        hist.push(u.peak_bytes);
        if hist.len() > HISTORY_LEN {
            let excess = hist.len() - HISTORY_LEN;
            hist.drain(..excess);
        }
        ended.push(Ended {
            key,
            repo: u.repo,
            peak_bytes: u.peak_bytes,
        });
    }
    store.sampled_at = Some(now);
    ended
}

/// Record an admitted dispatch as a pending unit (#11191), so it is reserved
/// for before the next sample sees it. An existing unit under `key` (a
/// re-dispatch of the same issue) restarts at zero.
pub fn record_pending(store: &mut Store, key: &str, repo: &str, issue: Option<u32>, now: i64) {
    store.inflight.insert(
        key.to_string(),
        Unit {
            repo: repo.to_string(),
            issue,
            current_bytes: 0,
            peak_bytes: 0,
            pending_since: Some(now),
        },
    );
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// A live claim lock (owner pid alive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    pub issue: u32,
    pub owner_pid: u32,
    pub pgid: Option<u32>,
}

/// A live Loom run dir (owner pid alive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDir {
    pub path: PathBuf,
    pub owner_pid: u32,
    pub pgid: Option<u32>,
}

/// Whether `pid` names a live process (`kill(pid, 0)`; `EPERM` is alive).
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().filter(|&p| p > 0) else {
        return false;
    };
    // SAFETY: signal 0 performs the existence and permission check only.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn pgid_of(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok().filter(|&p| p > 0)?;
    // SAFETY: getpgid only reads the process table.
    let pgid = unsafe { libc::getpgid(pid) };
    u32::try_from(pgid).ok().filter(|&g| g > 1)
}

#[derive(Deserialize)]
struct LockOwnerLite {
    issue: u32,
    owner_pid: u32,
    #[serde(default)]
    pgid: Option<u32>,
}

/// The live claim locks under `<root>/.loom/locks`.
#[must_use]
pub fn live_locks(root: &Path, alive: &dyn Fn(u32) -> bool) -> Vec<LockInfo> {
    let Ok(entries) = std::fs::read_dir(root.join(".loom").join("locks")) else {
        return Vec::new();
    };
    let mut out: Vec<LockInfo> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("issue-"))
        .filter_map(|e| std::fs::read_to_string(e.path().join("owner.json")).ok())
        .filter_map(|raw| serde_json::from_str::<LockOwnerLite>(&raw).ok())
        .filter(|o| alive(o.owner_pid))
        .map(|o| LockInfo {
            issue: o.issue,
            owner_pid: o.owner_pid,
            pgid: o.pgid,
        })
        .collect();
    out.sort_by_key(|l| l.issue);
    out
}

/// The live run dirs under `<root>/.loom/targets` (owner recorded and alive).
#[must_use]
pub fn live_run_dirs(
    root: &Path,
    alive: &dyn Fn(u32) -> bool,
    pgid: &dyn Fn(u32) -> Option<u32>,
) -> Vec<RunDir> {
    let Ok(entries) = std::fs::read_dir(crate::run_target_dir::targets_root(root)) else {
        return Vec::new();
    };
    let mut out: Vec<RunDir> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| crate::run_target_dir::is_run_target_dir(p))
        .filter_map(|path| {
            let owner_pid = crate::run_target_dir::owner_pid(&path).filter(|&p| alive(p))?;
            Some(RunDir {
                pgid: pgid(owner_pid),
                path,
                owner_pid,
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Assemble one repo's live units from its locks and run dirs (pure apart
/// from `size_of`). Each run dir belongs to the first lock whose pid owns it
/// or whose process group it is in; the rest are units of their own.
#[must_use]
pub fn assemble(
    repo: &str,
    worktree_root: &Path,
    locks: &[LockInfo],
    run_dirs: &[RunDir],
    size_of: &dyn Fn(&Path) -> u64,
) -> Vec<LiveUnit> {
    let mut claimed = vec![false; run_dirs.len()];
    let mut out = Vec::new();
    for lock in locks {
        let mut bytes = size_of(&worktree_root.join(format!("issue-{}", lock.issue)));
        for (i, rd) in run_dirs.iter().enumerate() {
            let same_group = lock.pgid.is_some() && rd.pgid == lock.pgid;
            if !claimed[i] && (rd.owner_pid == lock.owner_pid || same_group) {
                claimed[i] = true;
                bytes = bytes.saturating_add(size_of(&rd.path));
            }
        }
        out.push(LiveUnit {
            key: issue_key(repo, lock.issue),
            repo: repo.to_string(),
            issue: Some(lock.issue),
            bytes,
        });
    }
    for (rd, _) in run_dirs.iter().zip(&claimed).filter(|(_, c)| !**c) {
        let name = rd
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        out.push(LiveUnit {
            key: format!("{repo}:{name}"),
            repo: repo.to_string(),
            issue: None,
            bytes: size_of(&rd.path),
        });
    }
    out
}

/// Measure the live units of one workspace root.
#[must_use]
pub fn live_units(root: &Path) -> Vec<LiveUnit> {
    let repo = crate::ram_peaks::repo_key(root);
    let locks = live_locks(root, &pid_alive);
    let run_dirs = live_run_dirs(root, &pid_alive, &pgid_of);
    let size_of = |p: &Path| crate::worktree_disk_status::dir_size_bytes(p).unwrap_or(0);
    assemble(&repo, &crate::worktree_root::worktree_root(root), &locks, &run_dirs, &size_of)
}

// ---------------------------------------------------------------------------
// Persistence and the sampler
// ---------------------------------------------------------------------------

/// Store location: `$LOOM_DISK_FOOTPRINT_PATH`, else
/// `$HOME/.loom/disk-footprints.json`.
#[must_use]
pub fn store_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(STORE_PATH_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".loom")
            .join("disk-footprints.json"),
    )
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

/// Serializes this process's read-modify-write cycles on the store (the
/// sampler thread and the dispatch seam).
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Load, mutate and (when changed) persist the store under the process lock.
/// `None` when no store path resolves.
pub fn with_store<R>(f: impl FnOnce(&mut Store) -> R) -> Option<R> {
    let path = store_path()?;
    let _guard = STORE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut store = load(&path);
    let before = store.clone();
    let out = f(&mut store);
    if store != before {
        save(&path, &store);
    }
    Some(out)
}

/// The current store (empty when no path resolves).
#[must_use]
pub fn load_current() -> Store {
    store_path().map(|p| load(&p)).unwrap_or_default()
}

/// Unix seconds now.
#[must_use]
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// One sampling pass over `roots`: measure, fold, persist, log ended units.
pub fn sample_once(roots: &[PathBuf]) {
    let live: Vec<LiveUnit> = roots.iter().flat_map(|r| live_units(r)).collect();
    let ended = with_store(|s| observe(s, &live, now_secs())).unwrap_or_default();
    for e in ended {
        log::info!(
            "disk_footprint: {} (repo {}) ended at a peak of {:.2} GiB; folded into the repo's \
             disk charge history (#11191)",
            e.key,
            e.repo,
            e.peak_bytes as f64 / GIB as f64
        );
    }
}

static SAMPLER_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static SAMPLER: OnceLock<()> = OnceLock::new();

fn sample_interval() -> std::time::Duration {
    let secs = std::env::var(SAMPLE_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_SAMPLE_SECS);
    std::time::Duration::from_secs(secs)
}

/// Hand the sampler the current workspace roots, starting it on first call.
/// The walk runs on its own thread so a large target dir never stalls the
/// work-finder loop.
pub fn publish_roots(roots: &[PathBuf]) {
    *SAMPLER_ROOTS.lock().unwrap_or_else(PoisonError::into_inner) = roots.to_vec();
    SAMPLER.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("disk-footprint".into())
            .spawn(|| loop {
                let roots = SAMPLER_ROOTS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                sample_once(&roots);
                std::thread::sleep(sample_interval());
            });
        if let Err(e) = spawned {
            log::warn!("disk_footprint: could not start the sampler thread: {e}");
        }
    });
}

#[cfg(test)]
#[path = "disk_footprint_tests.rs"]
mod tests;
