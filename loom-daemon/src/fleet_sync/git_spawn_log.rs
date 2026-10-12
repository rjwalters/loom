//! Attribution and memory bounds for the git children the fleet-sync helpers
//! spawn (#11059).
//!
//! A git child of the daemon once grew to ~16 GB RSS and OOMed a worker, and
//! nothing recorded which command it was. Every git the three fleet-sync
//! helpers (`workspace_resync`, `checkout_ff`, `roster_clone`) run goes through
//! [`run_logged`], which logs argv, repo root and a monotonic spawn id at start
//! and status, wall time and peak RSS at exit, and keeps the last
//! [`RING_CAPACITY`] spawns in memory ([`snapshot`]). Only argv is recorded,
//! never the environment, so credentials set by `apply_gh_config_for_root`
//! cannot leak.
//!
//! Peak RSS is sampled from `/proc` on Linux (the largest `VmHWM` over the
//! child's process group, so `index-pack` and `pack-objects` grandchildren
//! count). It is `None` elsewhere.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::proc_exec::{run_bounded_observed, Completion, ExecError};

/// How many recent spawns the in-memory ring keeps.
pub const RING_CAPACITY: usize = 64;

/// Memory-bounding `-c` pairs for `fetch` and `clone`: they cap
/// `pack-objects`/`index-pack` memory without a cgroup. One place to tune.
pub const MEMORY_BOUND_CONFIG: &[&str] = &[
    "pack.windowMemory=256m",
    "pack.threads=1",
    "core.packedGitLimit=512m",
    "core.deltaBaseCacheLimit=96m",
];

/// The memory bound for git subcommand `sub`: `fetch` and `clone` only.
#[must_use]
pub fn memory_config_for(sub: &str) -> &'static [&'static str] {
    if matches!(sub, "fetch" | "clone") {
        MEMORY_BOUND_CONFIG
    } else {
        &[]
    }
}

/// How a spawn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitRecord {
    /// Exit status text, or `timed out` / `error: ...`.
    pub status: String,
    /// Wall time.
    pub wall: Duration,
    /// Peak RSS in KiB of the child's process group, when measurable.
    pub peak_rss_kib: Option<u64>,
}

/// One git spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRecord {
    /// Monotonic per-process spawn id.
    pub id: u64,
    /// Which helper spawned it.
    pub site: &'static str,
    /// `git` plus its arguments (no environment).
    pub argv: Vec<String>,
    /// The registered workspace root (or clone destination).
    pub root: String,
    /// `None` while running.
    pub exit: Option<ExitRecord>,
}

/// A bounded ring of [`SpawnRecord`]s.
#[derive(Debug)]
pub struct Ring {
    cap: usize,
    items: VecDeque<SpawnRecord>,
}

impl Ring {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            items: VecDeque::with_capacity(cap),
        }
    }

    /// Append, evicting the oldest past capacity.
    pub fn push(&mut self, rec: SpawnRecord) {
        while self.items.len() >= self.cap.max(1) {
            self.items.pop_front();
        }
        self.items.push_back(rec);
    }

    /// Attach the exit to spawn `id`, if it is still in the ring.
    pub fn finish(&mut self, id: u64, exit: ExitRecord) {
        if let Some(r) = self.items.iter_mut().find(|r| r.id == id) {
            r.exit = Some(exit);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn records(&self) -> Vec<SpawnRecord> {
        self.items.iter().cloned().collect()
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn ring() -> &'static Mutex<Ring> {
    static RING: OnceLock<Mutex<Ring>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(Ring::new(RING_CAPACITY)))
}

/// The most recent git spawns, oldest first. For a crash or OOM report.
#[must_use]
pub fn snapshot() -> Vec<SpawnRecord> {
    ring().lock().map(|r| r.records()).unwrap_or_default()
}

/// `git` and its arguments as built on `cmd`; the environment is not read.
#[must_use]
pub fn argv_of(cmd: &Command) -> Vec<String> {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// Run `cmd` (a git command) under `timeout`, logging and recording it.
///
/// # Errors
/// As [`run_bounded_observed`].
pub fn run_logged(
    site: &'static str,
    root: &Path,
    cmd: Command,
    timeout: Duration,
) -> Result<Completion, ExecError> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let argv = argv_of(&cmd);
    let root_s = root.display().to_string();
    log::info!("git_spawn start id={id} site={site} root={root_s} argv={argv:?}");
    if let Ok(mut r) = ring().lock() {
        r.push(SpawnRecord {
            id,
            site,
            argv,
            root: root_s.clone(),
            exit: None,
        });
    }
    let started = Instant::now();
    let sampler = Sampler::new();
    let result = run_bounded_observed(cmd, timeout, |pid| sampler.start(pid));
    let peak = sampler.stop();
    let status = match &result {
        Ok(Completion::Exited(out)) => out.status.to_string(),
        Ok(Completion::TimedOut { .. }) => "timed out".to_string(),
        Err(e) => format!("error: {e}"),
    };
    let exit = ExitRecord {
        status,
        wall: started.elapsed(),
        peak_rss_kib: peak,
    };
    log::info!(
        "git_spawn exit id={id} site={site} root={root_s} status={} wall_ms={} peak_rss_kib={}",
        exit.status,
        exit.wall.as_millis(),
        exit.peak_rss_kib
            .map_or_else(|| "unknown".to_string(), |k| k.to_string()),
    );
    if let Ok(mut r) = ring().lock() {
        r.finish(id, exit);
    }
    result
}

/// Samples the peak RSS of a child's process group while it runs.
struct Sampler {
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Sampler {
    fn new() -> Self {
        Self {
            peak: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    #[cfg(target_os = "linux")]
    fn start(&self, pid: u32) {
        // Sample once synchronously: a short-lived git may be gone before the
        // thread's first pass.
        sample_group(pid, &self.peak);
        let (peak, stop) = (Arc::clone(&self.peak), Arc::clone(&self.stop));
        let h = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                sample_group(pid, &peak);
            }
        });
        if let Ok(mut slot) = self.handle.lock() {
            *slot = Some(h);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn start(&self, _pid: u32) {}

    fn stop(&self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.lock().ok().and_then(|mut s| s.take()) {
            let _ = h.join();
        }
        match self.peak.load(Ordering::Relaxed) {
            0 => None,
            k => Some(k),
        }
    }
}

/// Raise `peak` to the largest `VmHWM` (KiB) among processes whose process
/// group is `pgid`. The child is its own group leader (`proc_exec`).
#[cfg(target_os = "linux")]
fn sample_group(pgid: u32, peak: &AtomicU64) {
    // Read the leader by pid first: the full /proc walk reaches a short-lived
    // child last, by which time it may be a zombie with no VmHWM line.
    read_vmhwm(&pgid.to_string(), peak);
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `pid (comm) state ppid pgrp ...`; comm may contain spaces/parens.
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            continue;
        };
        if rest
            .split_whitespace()
            .nth(2)
            .and_then(|g| g.parse::<u32>().ok())
            != Some(pgid)
        {
            continue;
        }
        read_vmhwm(pid, peak);
    }
}

/// Raise `peak` to `/proc/<pid>/status` `VmHWM` (KiB); a gone or zombie pid
/// has no such line and is skipped.
#[cfg(target_os = "linux")]
fn read_vmhwm(pid: &str, peak: &AtomicU64) {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return;
    };
    let hwm = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(k) = hwm {
        peak.fetch_max(k, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: u64) -> SpawnRecord {
        SpawnRecord {
            id,
            site: "t",
            argv: vec!["git".into()],
            root: "/r".into(),
            exit: None,
        }
    }

    #[test]
    fn ring_keeps_exactly_capacity() {
        let mut ring = Ring::new(RING_CAPACITY);
        for i in 0..(RING_CAPACITY as u64 + 10) {
            ring.push(rec(i));
        }
        assert_eq!(ring.len(), RING_CAPACITY);
        assert_eq!(ring.records()[0].id, 10);
    }

    #[test]
    fn only_fetch_and_clone_are_memory_bounded() {
        assert_eq!(memory_config_for("fetch"), MEMORY_BOUND_CONFIG);
        assert_eq!(memory_config_for("clone"), MEMORY_BOUND_CONFIG);
        assert!(memory_config_for("rev-parse").is_empty());
    }

    #[test]
    fn run_logged_records_start_and_exit_with_argv_root_and_id() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cmd = Command::new("git");
        cmd.args(["-c", "alias.nap=!sleep 0.3", "nap"])
            .env("SECRET_TOKEN", "hunter2");
        let out = run_logged("test", tmp.path(), cmd, Duration::from_secs(20)).unwrap();
        assert!(matches!(out, Completion::Exited(o) if o.status.success()));
        let root = tmp.path().display().to_string();
        let mine: Vec<_> = snapshot().into_iter().filter(|r| r.root == root).collect();
        assert_eq!(mine.len(), 1);
        let r = &mine[0];
        assert_eq!(r.argv, ["git", "-c", "alias.nap=!sleep 0.3", "nap"]);
        assert!(r.id > 0);
        assert!(!format!("{r:?}").contains("hunter2"));
        let exit = r.exit.as_ref().expect("exit record");
        assert!(exit.status.contains('0'));
        #[cfg(target_os = "linux")]
        assert!(exit.peak_rss_kib.is_some_and(|k| k > 0), "{exit:?}");
    }
}
