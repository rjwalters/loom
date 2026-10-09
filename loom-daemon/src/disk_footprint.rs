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
//!   its worktree (`<worktree-root>/issue-N`), the worktree's #8458
//!   per-worktree target dir when its `.loom-cargo-target-dir` marker names
//!   one, and every live Loom run dir (`<repo>/.loom/targets/<role>-<run>`,
//!   #8370) whose owner pid is the lock's pid or in the lock's process group.
//!   Key `<repo>#<N>`.
//! - A **PR-set sweep** (live `<repo>/.loom/locks/pr-N` locks sharing one
//!   owner pid): the run dirs that owner holds. Key `<repo>#prs-<lowest N>`,
//!   the key admission records it under.
//! - Any **other live run dir**: the dir alone, key `<repo>:<dir name>`. Its
//!   role is read from the dir name ([`run_dir_role`]). A sweep role
//!   ([`SWEEP_ROLES`]) is still a sweep; anything else (a curator, champion,
//!   judge or guide tick) is a **role run**.
//!
//! # Two histories (#11191 review)
//!
//! Sweeps fold into the per-repo history ([`Store::repos`]) that sets the
//! repo's charge. Role runs never do: a curator tick's run dir holds only its
//! owner file, and twenty of them would otherwise age a 26 GB build out of
//! the window. They fold into a per-`<repo>:<role>` history
//! ([`Store::role_runs`]) instead, and admission reserves a live role run
//! against its own role's mark, not the repo's sweep charge.
//!
//! A unit is folded only when its peak is at least [`MIN_FOLD_BYTES`] (a run
//! dir holding only its owner file never built) and its build output was
//! measured at least once. A sweep in a Cargo repo whose build went somewhere
//! no probe can see (an operator `CARGO_TARGET_DIR`, a containerized run) is
//! not folded, and a WARN names the repo once, so the repo keeps its
//! configured or default charge instead of learning a source-only footprint.
//!
//! Admission also records a *pending* unit the moment it admits a dispatch
//! ([`record_pending`]), so a sweep that no sample has seen yet is reserved
//! for. A pending unit lives [`PENDING_GRACE_SECS`] unless a sample adopts it.
//!
//! Every probe is best-effort: an unreadable lock or dir is skipped, never an
//! error. A repo with no history keeps its configured or default charge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

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

/// A unit whose peak stays below this never built anything (a role run's dir
/// holds only its owner file, a few bytes); it is not folded into any history.
pub const MIN_FOLD_BYTES: u64 = 1024 * 1024;

/// Run-dir roles that are sweeps (fold into the repo's sweep history), as
/// `worker_spawn` names a derived dir from `LOOM_ROLE`. Every other role is a
/// role run.
pub const SWEEP_ROLES: &[&str] = &["sweep-lifecycle", "sweep", "loom", "builder"];

/// The role a run dir was created for, read from its name: a role-runner
/// tick's `<role>-role-<role>-<time>-<rand>`, or a derived
/// `<role>-<pid>-<time>` (trailing all-digit segments dropped).
#[must_use]
pub fn run_dir_role(name: &str) -> String {
    if let Some((role, _)) = name.split_once("-role-") {
        return role.to_string();
    }
    let mut s = name;
    while let Some((head, tail)) = s.rsplit_once('-') {
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        s = head;
    }
    s.to_string()
}

/// The role-run role of the unit under `key`, or `None` for a sweep. Derived
/// from the key alone, so a store written before roles mattered classifies
/// the same way: `<repo>#...` is a sweep, `<repo>:<dir>` is a role run unless
/// its dir names a [`SWEEP_ROLES`] role.
#[must_use]
pub fn role_of_key(key: &str) -> Option<String> {
    if key.contains('#') {
        return None;
    }
    let name = key.rsplit_once(':').map_or(key, |(_, n)| n);
    let role = run_dir_role(name);
    (!SWEEP_ROLES.contains(&role.as_str())).then_some(role)
}

/// The [`Store::role_runs`] key for `role` in `repo`.
#[must_use]
pub fn role_key(repo: &str, role: &str) -> String {
    format!("{repo}:{role}")
}

fn yes() -> bool {
    true
}

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
    /// Whether any sample measured this unit's build output (always true for
    /// a unit with nothing to build). A unit that ends without it is not
    /// folded into history.
    #[serde(default = "yes")]
    pub build_measured: bool,
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
}

/// On-disk state: per-repo footprint history plus the in-flight units.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    /// Repo -> recent per-sweep peak footprints (bytes), oldest first.
    #[serde(default)]
    pub repos: BTreeMap<String, Vec<u64>>,
    /// `<repo>:<role>` -> recent role-run peak footprints (bytes), oldest
    /// first. Kept apart from `repos` so role ticks never wash out a sweep's
    /// mark (#11191 review).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub role_runs: BTreeMap<String, Vec<u64>>,
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

    /// The observed high-water mark (bytes) of `role` runs in `repo`.
    #[must_use]
    pub fn role_high_water_bytes(&self, repo: &str, role: &str) -> Option<u64> {
        self.role_runs
            .get(&role_key(repo, role))
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
    /// `bytes` covers the unit's build output (or it has none to build).
    pub build_measured: bool,
}

/// A unit that was live and is gone now, with the peak folded into history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub key: String,
    pub repo: String,
    pub peak_bytes: u64,
    /// Where the peak went: `Some(history key)` when folded, `None` when not.
    pub folded_into: Option<String>,
    /// Not folded because its build output was never measured.
    pub unmeasured: bool,
}

fn push_history(hist: &mut Vec<u64>, peak: u64) {
    hist.push(peak);
    if hist.len() > HISTORY_LEN {
        let excess = hist.len() - HISTORY_LEN;
        hist.drain(..excess);
    }
}

/// The key of an issue sweep's unit.
#[must_use]
pub fn issue_key(repo: &str, issue: u32) -> String {
    format!("{repo}#{issue}")
}

/// Fold one sample into `store`. Live units are upserted (peak is monotonic,
/// and a sample adopts a pending unit of the same key). A tracked unit that
/// is not live is dropped: a pending one only once its grace has run out, and
/// without touching history; a sampled one is returned, and its peak folded
/// into its sweep or role history unless it never built ([`MIN_FOLD_BYTES`])
/// or its build output was never measured.
pub fn observe(store: &mut Store, live: &[LiveUnit], now: i64) -> Vec<Ended> {
    for l in live {
        let prev = store
            .inflight
            .get(&l.key)
            .filter(|u| u.pending_since.is_none());
        let prev_peak = prev.map_or(0, |u| u.peak_bytes);
        let prev_measured = prev.is_some_and(|u| u.build_measured);
        store.inflight.insert(
            l.key.clone(),
            Unit {
                repo: l.repo.clone(),
                issue: l.issue,
                current_bytes: l.bytes,
                peak_bytes: l.bytes.max(prev_peak),
                pending_since: None,
                build_measured: l.build_measured || prev_measured,
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
        let role = (u.issue.is_none()).then(|| role_of_key(&key)).flatten();
        let unmeasured = !u.build_measured;
        let folded_into = (u.peak_bytes >= MIN_FOLD_BYTES && !unmeasured).then(|| {
            let (map, hkey) = match &role {
                Some(role) => (&mut store.role_runs, role_key(&u.repo, role)),
                None => (&mut store.repos, u.repo.clone()),
            };
            push_history(map.entry(hkey.clone()).or_default(), u.peak_bytes);
            hkey
        });
        ended.push(Ended {
            key,
            repo: u.repo,
            peak_bytes: u.peak_bytes,
            folded_into,
            unmeasured,
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
            build_measured: true,
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

/// The live issue claim locks (`issue-N`) under `<root>/.loom/locks`.
#[must_use]
pub fn live_locks(root: &Path, alive: &dyn Fn(u32) -> bool) -> Vec<LockInfo> {
    live_locks_named(root, "issue-", alive)
}

/// The live PR-set member locks (`pr-N`) under `<root>/.loom/locks`; `issue`
/// holds the PR number.
#[must_use]
pub fn live_pr_locks(root: &Path, alive: &dyn Fn(u32) -> bool) -> Vec<LockInfo> {
    live_locks_named(root, "pr-", alive)
}

fn live_locks_named(root: &Path, prefix: &str, alive: &dyn Fn(u32) -> bool) -> Vec<LockInfo> {
    let Ok(entries) = std::fs::read_dir(root.join(".loom").join("locks")) else {
        return Vec::new();
    };
    let mut out: Vec<LockInfo> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
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

/// One repo's probe results, for [`assemble`].
#[derive(Debug, Clone, Copy)]
pub struct RepoScan<'a> {
    pub repo: &'a str,
    pub worktree_root: &'a Path,
    /// The repo root has a `Cargo.toml`, so its sweeps build (the same test
    /// `run_target_dir::decide` uses).
    pub builds: bool,
    pub locks: &'a [LockInfo],
    pub pr_locks: &'a [LockInfo],
    pub run_dirs: &'a [RunDir],
}

/// The filesystem probes [`assemble`] uses, injected for tests.
pub struct Probes<'a> {
    /// Bytes under a path (0 when unreadable).
    pub size_of: &'a dyn Fn(&Path) -> u64,
    /// A worktree's #8458 per-worktree target dir, from its
    /// `.loom-cargo-target-dir` marker.
    pub worktree_target: &'a dyn Fn(&Path) -> Option<PathBuf>,
}

/// Assemble one repo's live units from its locks and run dirs (pure apart
/// from the probes). Each run dir belongs to the first lock (issue, then PR
/// set) whose pid owns it or whose process group it is in; the rest are units
/// of their own.
#[must_use]
pub fn assemble(scan: &RepoScan<'_>, probes: &Probes<'_>) -> Vec<LiveUnit> {
    let RepoScan {
        repo,
        worktree_root,
        builds,
        locks,
        pr_locks,
        run_dirs,
    } = *scan;
    let mut claimed = vec![false; run_dirs.len()];
    // Sum the unclaimed run dirs `owner` holds; returns (bytes, any found).
    let mut claim = |owner_pid: u32, pgid: Option<u32>| -> (u64, bool) {
        let (mut bytes, mut found) = (0u64, false);
        for (i, rd) in run_dirs.iter().enumerate() {
            let same_group = pgid.is_some() && rd.pgid == pgid;
            if !claimed[i] && (rd.owner_pid == owner_pid || same_group) {
                claimed[i] = true;
                found = true;
                bytes = bytes.saturating_add((probes.size_of)(&rd.path));
            }
        }
        (bytes, found)
    };
    let mut out = Vec::new();
    for lock in locks {
        let worktree = worktree_root.join(format!("issue-{}", lock.issue));
        let mut bytes = (probes.size_of)(&worktree);
        // #8458: a per-worktree target dir lives outside the worktree.
        let marker = (probes.worktree_target)(&worktree).filter(|t| !t.starts_with(&worktree));
        if let Some(target) = &marker {
            bytes = bytes.saturating_add((probes.size_of)(target));
        }
        let (run_bytes, run_found) = claim(lock.owner_pid, lock.pgid);
        out.push(LiveUnit {
            key: issue_key(repo, lock.issue),
            repo: repo.to_string(),
            issue: Some(lock.issue),
            bytes: bytes.saturating_add(run_bytes),
            build_measured: !builds || marker.is_some() || run_found,
        });
    }
    // PR sets: one unit per owner, keyed by its lowest PR.
    let mut sets: BTreeMap<u32, (u32, Option<u32>)> = BTreeMap::new();
    for l in pr_locks {
        let e = sets.entry(l.owner_pid).or_insert((l.issue, l.pgid));
        e.0 = e.0.min(l.issue);
    }
    for (owner_pid, (first, pgid)) in sets {
        let (bytes, found) = claim(owner_pid, pgid);
        out.push(LiveUnit {
            key: format!("{repo}#prs-{first}"),
            repo: repo.to_string(),
            issue: None,
            bytes,
            build_measured: !builds || found,
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
            bytes: (probes.size_of)(&rd.path),
            build_measured: true,
        });
    }
    out
}

/// Measure the live units of one workspace root.
///
/// Sizes are `dir_size_bytes`: apparent file lengths, with a hard-linked file
/// counted once per link (cargo uplifts each final binary as a hard link into
/// `deps/`). Both err toward a larger footprint, so the charge errs toward
/// over-reserving, the safe direction.
#[must_use]
pub fn live_units(root: &Path) -> Vec<LiveUnit> {
    let repo = crate::ram_peaks::repo_key(root);
    let locks = live_locks(root, &pid_alive);
    let pr_locks = live_pr_locks(root, &pid_alive);
    let run_dirs = live_run_dirs(root, &pid_alive, &pgid_of);
    let worktree_root = crate::worktree_root::worktree_root(root);
    let builds = root.join("Cargo.toml").is_file();
    if builds {
        warn_if_ambient_target(&repo);
    }
    let size_of = |p: &Path| crate::worktree_disk_status::dir_size_bytes(p).unwrap_or(0);
    let worktree_target =
        |wt: &Path| crate::worktree_ops::cargo_target::per_worktree::marker_value(wt);
    let scan = RepoScan {
        repo: &repo,
        worktree_root: &worktree_root,
        builds,
        locks: &locks,
        pr_locks: &pr_locks,
        run_dirs: &run_dirs,
    };
    assemble(
        &scan,
        &Probes {
            size_of: &size_of,
            worktree_target: &worktree_target,
        },
    )
}

/// Repos already warned about unmeasurable build output (once per repo per
/// daemon process).
static UNMEASURED_WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Whether `repo` has not been warned about yet (and mark it warned).
fn first_unmeasured_warning(repo: &str) -> bool {
    UNMEASURED_WARNED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(repo.to_string())
}

fn unmeasured_warning(repo: &str, why: &str) -> String {
    format!(
        "disk_footprint: {repo}'s build output cannot be measured ({why}); its sweeps are not \
         folded into its disk charge history, so it is charged its configured \
         `{}` or the default `LOOM_PER_WORKTREE_GB`. Set `{}` in {repo}'s \
         .loom/config.json to its real per-sweep footprint (#11191)",
        crate::disk_admission::REPO_CHARGE_KEY,
        crate::disk_admission::REPO_CHARGE_KEY,
    )
}

/// An operator `CARGO_TARGET_DIR` in the daemon's environment reaches every
/// spawn (`run_target_dir` precedence 2): the builds go to one shared dir no
/// unit can be charged for. Warn once per Cargo repo.
fn warn_if_ambient_target(repo: &str) {
    let Some(dir) = std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) else {
        return;
    };
    if first_unmeasured_warning(repo) {
        let why = format!(
            "an operator CARGO_TARGET_DIR={} is shared by every sweep",
            dir.to_string_lossy()
        );
        log::warn!("{}", unmeasured_warning(repo, &why));
    }
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

/// Load the store. A missing file is an empty store. A corrupt one is also
/// an empty store, but never silently: it is moved aside to
/// `<path>.corrupt-<unix secs>` (so the next save cannot overwrite the
/// evidence) and a WARN names both paths, since every repo's charge drops to
/// its configured or default value until history rebuilds.
#[must_use]
pub fn load(path: &Path) -> Store {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Store::default();
    };
    match serde_json::from_str(&raw) {
        Ok(store) => store,
        Err(e) => {
            let aside = PathBuf::from(format!("{}.corrupt-{}", path.display(), now_secs()));
            let moved = std::fs::rename(path, &aside);
            log::warn!(
                "disk_footprint: {} is corrupt ({e}); {}; starting from an empty store, so \
                 every repo is charged its configured or default disk charge until its \
                 history rebuilds (#11191)",
                path.display(),
                match moved {
                    Ok(()) => format!("moved it aside to {}", aside.display()),
                    Err(re) => format!("could not move it aside to {}: {re}", aside.display()),
                }
            );
            Store::default()
        }
    }
}

/// Atomically persist the store (best effort). The temp file is named for
/// this process, so a second process writing the same store (a CLI path;
/// [`STORE_LOCK`] is process-local) never shares a half-written temp.
pub fn save(path: &Path, store: &Store) {
    let Ok(json) = serde_json::to_string_pretty(store) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
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
        let gib = e.peak_bytes as f64 / GIB as f64;
        match &e.folded_into {
            Some(h) => log::info!(
                "disk_footprint: {} (repo {}) ended at a peak of {gib:.2} GiB; folded into \
                 the {h} disk history (#11191)",
                e.key,
                e.repo,
            ),
            None if e.unmeasured => {
                if first_unmeasured_warning(&e.repo) {
                    let why = format!(
                        "{} ended without its target dir ever being found: no Loom run dir, no \
                         per-worktree marker; an operator CARGO_TARGET_DIR or a containerized run",
                        e.key
                    );
                    log::warn!("{}", unmeasured_warning(&e.repo, &why));
                } else {
                    log::debug!(
                        "disk_footprint: {} ended with its build output unmeasured; not folded",
                        e.key
                    );
                }
            }
            None => log::debug!(
                "disk_footprint: {} ended at {} bytes, below the fold floor; not folded",
                e.key,
                e.peak_bytes
            ),
        }
    }
}

static SAMPLER_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static SAMPLER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn sample_interval() -> std::time::Duration {
    let secs = std::env::var(SAMPLE_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_SAMPLE_SECS);
    std::time::Duration::from_secs(secs)
}

/// One sampler iteration, with a panic caught and logged: a bad probe costs
/// one sample, never the thread.
fn sample_guarded(roots: &[PathBuf]) {
    sample_guarded_with(roots, &sample_once);
}

fn sample_guarded_with(roots: &[PathBuf], sample: &dyn Fn(&[PathBuf])) {
    let run = std::panic::AssertUnwindSafe(|| sample(roots));
    if let Err(panic) = std::panic::catch_unwind(run) {
        let what = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic".to_string());
        log::error!("disk_footprint: a sample panicked ({what}); retrying next interval (#11191)");
    }
}

/// Hand the sampler the current workspace roots, starting it on first call,
/// and restarting it if it has died. The walk runs on its own thread so a
/// large target dir never stalls the work-finder loop.
pub fn publish_roots(roots: &[PathBuf]) {
    *SAMPLER_ROOTS.lock().unwrap_or_else(PoisonError::into_inner) = roots.to_vec();
    let mut sampler = SAMPLER.lock().unwrap_or_else(PoisonError::into_inner);
    let restart = match sampler.as_ref() {
        None => false,
        Some(h) if h.is_finished() => true,
        Some(_) => return,
    };
    if restart {
        log::warn!("disk_footprint: the sampler thread had stopped; restarting it (#11191)");
    }
    let spawned = std::thread::Builder::new()
        .name("disk-footprint".into())
        .spawn(|| loop {
            let roots = SAMPLER_ROOTS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            sample_guarded(&roots);
            std::thread::sleep(sample_interval());
        });
    match spawned {
        Ok(h) => *sampler = Some(h),
        Err(e) => log::warn!("disk_footprint: could not start the sampler thread: {e}"),
    }
}

#[cfg(test)]
#[path = "disk_footprint_tests.rs"]
mod tests;
