//! Cgroup-derived census of live `loom-agent-*` scopes for `host.health`
//! (Issue #12166).
//!
//! `host.health.active_sweep_ids` comes from the daemon's in-memory registry,
//! so a scope that outlives its registry entry (daemon restart, a dropped
//! stalled sweep) is invisible there. This module reads the same scopes from
//! the cgroup tree instead, reusing [`crate::ram_peaks::list_agent_scopes`]
//! (which already skips probe scopes), and reports per scope whether an agent
//! process is still alive, the oldest leftover process, cumulative CPU time and
//! the age of the newest transcript write. Observation only: reaping residue is
//! #10802.
//!
//! Every per-scope measurement is `Option`: unmeasurable stays absent, never a
//! fabricated zero (the `host.health` omission contract).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::telemetry::AgentScopeSummary;

/// Most scopes listed in one record; the true total rides separately so a
/// runaway host cannot bloat `host.health`.
pub const MAX_AGENT_SCOPES: usize = 50;

/// Host probes the census needs, injectable so tests can use a fixture tree.
pub struct ProcProbe<'a> {
    /// `/proc/<pid>/comm`, trimmed; `None` once the process is gone.
    pub comm: &'a dyn Fn(u32) -> Option<String>,
    /// Seconds since the process started; `None` when unreadable.
    pub age_sec: &'a dyn Fn(u32) -> Option<u64>,
    /// The process's working directory.
    pub cwd: &'a dyn Fn(u32) -> Option<PathBuf>,
    /// Newest transcript write for a working directory.
    pub transcript_mtime: &'a dyn Fn(&Path) -> Option<SystemTime>,
}

/// Whether `comm` names an agent runtime / its launcher rather than a
/// leftover child (build tool, `sleep`, a stray `ngspice`).
#[must_use]
pub fn is_agent_comm(comm: &str) -> bool {
    comm.starts_with("claude") || comm.starts_with("spawn-claude") || comm.starts_with("codex")
}

fn since(now: SystemTime, t: SystemTime) -> Option<u64> {
    now.duration_since(t).ok().map(|d| d.as_secs())
}

/// `usage_usec` from a cgroup `cpu.stat` body.
#[must_use]
pub fn parse_cpu_usage_usec(body: &str) -> Option<u64> {
    body.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == "usage_usec").then(|| v.trim().parse().ok())?
    })
}

/// Summarise one scope under `slice_dir`. Never fails: unreadable pieces are
/// simply absent (a scope with an unreadable `cgroup.procs` reports
/// `agent_alive=false` and no process detail).
#[must_use]
pub fn summarize_scope(
    slice_dir: &Path,
    scope: &str,
    now: SystemTime,
    probe: &ProcProbe<'_>,
) -> AgentScopeSummary {
    let dir = slice_dir.join(scope);
    let age_sec = std::fs::metadata(&dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| since(now, t));
    let pids: Vec<u32> = std::fs::read_to_string(dir.join("cgroup.procs"))
        .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default();
    // A pid can exit between listing and the /proc read: it simply drops out.
    let live: Vec<(u32, String)> = pids
        .iter()
        .filter_map(|&p| (probe.comm)(p).map(|c| (p, c)))
        .collect();
    let agent_alive = live.iter().any(|(_, c)| is_agent_comm(c));
    let oldest = live
        .iter()
        .filter_map(|(p, c)| (probe.age_sec)(*p).map(|a| (*p, c.clone(), a)))
        .max_by_key(|(_, _, a)| *a);
    let cpu_usec = std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()
        .and_then(|s| parse_cpu_usage_usec(&s));
    let transcript_age_sec = live
        .iter()
        .find_map(|(p, _)| (probe.cwd)(*p))
        .and_then(|cwd| (probe.transcript_mtime)(&cwd))
        .and_then(|t| since(now, t));
    AgentScopeSummary {
        scope: scope.to_string(),
        age_sec,
        agent_alive,
        oldest_pid: oldest.as_ref().map(|(p, _, _)| *p),
        oldest_comm: oldest.as_ref().map(|(_, c, _)| c.clone()),
        oldest_age_sec: oldest.map(|(_, _, a)| a),
        cpu_usec,
        transcript_age_sec,
    }
}

/// Census of every agent scope under `slice_dir`, capped at
/// [`MAX_AGENT_SCOPES`]; returns `(listed, total)`. `None` when the slice
/// directory does not exist (non-Linux, no systemd user manager): no field,
/// no error.
#[must_use]
pub fn census_in(
    slice_dir: &Path,
    now: SystemTime,
    probe: &ProcProbe<'_>,
) -> Option<(Vec<AgentScopeSummary>, u32)> {
    if !slice_dir.is_dir() {
        return None;
    }
    let scopes = crate::ram_peaks::list_agent_scopes(slice_dir);
    let total = u32::try_from(scopes.len()).unwrap_or(u32::MAX);
    let listed = scopes
        .iter()
        .take(MAX_AGENT_SCOPES)
        .map(|s| summarize_scope(slice_dir, s, now, probe))
        .collect();
    Some((listed, total))
}

/// Census of this host's live agent scopes (Linux; `None` elsewhere).
#[must_use]
pub fn census() -> Option<(Vec<AgentScopeSummary>, u32)> {
    #[cfg(target_os = "linux")]
    {
        let slice = crate::ram_peaks::agents_slice_dir()?;
        let projects = dirs::home_dir()?.join(".claude/projects");
        let comm = |p: u32| {
            std::fs::read_to_string(format!("/proc/{p}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
        };
        let age = |p: u32| proc_age_sec(p);
        let cwd = |p: u32| std::fs::read_link(format!("/proc/{p}/cwd")).ok();
        let transcript = |cwd: &Path| newest_mtime(&projects.join(encode_cwd(cwd)));
        census_in(
            &slice,
            SystemTime::now(),
            &ProcProbe {
                comm: &comm,
                age_sec: &age,
                cwd: &cwd,
                transcript_mtime: &transcript,
            },
        )
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// Claude Code's project-directory name for a cwd: every non-alphanumeric
/// character becomes `-`.
#[must_use]
pub fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Newest file mtime directly under `dir`.
#[must_use]
pub fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max()
}

#[cfg(target_os = "linux")]
fn proc_age_sec(pid: u32) -> Option<u64> {
    // Field 22 of /proc/<pid>/stat (clock ticks since boot), after the
    // parenthesised comm which may itself contain spaces.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let start_ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: sysconf is a pure query with no pointer arguments.
    let hz = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let started = Duration::from_secs(start_ticks.checked_div(hz)?);
    Duration::from_secs_f64(uptime)
        .checked_sub(started)
        .map(|d| d.as_secs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn mk_scope(root: &Path, name: &str, procs: &str, cpu: Option<&str>) {
        let d = root.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("cgroup.procs"), procs).unwrap();
        if let Some(c) = cpu {
            fs::write(d.join("cpu.stat"), c).unwrap();
        }
    }

    fn comm(p: u32) -> Option<String> {
        match p {
            10 => Some("claude".into()),
            20 => Some("ngspice".into()),
            _ => None, // 30 exits between listing and /proc read
        }
    }
    fn age(p: u32) -> Option<u64> {
        Some(u64::from(p) * 1000)
    }

    fn probe_with<R>(f: impl FnOnce(&ProcProbe<'_>) -> R) -> R {
        let cwd = |_: u32| Some(PathBuf::from("/w"));
        let tr = |_: &Path| Some(SystemTime::now() - Duration::from_secs(90));
        f(&ProcProbe {
            comm: &comm,
            age_sec: &age,
            cwd: &cwd,
            transcript_mtime: &tr,
        })
    }

    #[test]
    fn alive_agent_and_orphan_with_leftover_child() {
        let t = tempfile::tempdir().unwrap();
        mk_scope(
            t.path(),
            "loom-agent-a.scope",
            "10\n20\n30\n",
            Some("usage_usec 777\nuser_usec 1\n"),
        );
        mk_scope(t.path(), "loom-agent-b.scope", "20\n", None);
        mk_scope(t.path(), "loom-agent-probe-x.scope", "10\n", None);
        let (list, total) = probe_with(|p| census_in(t.path(), SystemTime::now(), p)).unwrap();
        assert_eq!(total, 2, "probe scope excluded");
        let a = &list[0];
        assert!(a.agent_alive);
        assert_eq!(a.cpu_usec, Some(777));
        assert_eq!(a.oldest_pid, Some(20));
        assert_eq!(a.oldest_comm.as_deref(), Some("ngspice"));
        assert!(a.transcript_age_sec.is_some());
        let b = &list[1];
        assert!(!b.agent_alive, "only a non-agent pid remains");
        assert_eq!(b.cpu_usec, None, "unreadable cpu.stat is absent, not zero");
        assert_eq!(b.oldest_age_sec, Some(20_000));
    }

    #[test]
    fn unreadable_procs_yields_empty_detail() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("loom-agent-c.scope")).unwrap();
        let (list, _) = probe_with(|p| census_in(t.path(), SystemTime::now(), p)).unwrap();
        assert!(!list[0].agent_alive);
        assert_eq!(list[0].oldest_pid, None);
        assert_eq!(list[0].transcript_age_sec, None);
    }

    #[test]
    fn missing_slice_is_none() {
        let t = tempfile::tempdir().unwrap();
        assert!(probe_with(|p| census_in(&t.path().join("nope"), SystemTime::now(), p)).is_none());
    }

    #[test]
    fn list_is_capped_but_total_is_true() {
        let t = tempfile::tempdir().unwrap();
        for i in 0..(MAX_AGENT_SCOPES + 5) {
            mk_scope(t.path(), &format!("loom-agent-{i:03}.scope"), "", None);
        }
        let (list, total) = probe_with(|p| census_in(t.path(), SystemTime::now(), p)).unwrap();
        assert_eq!(list.len(), MAX_AGENT_SCOPES);
        assert_eq!(total as usize, MAX_AGENT_SCOPES + 5);
    }

    #[test]
    fn encode_cwd_matches_claude_project_dirs() {
        assert_eq!(encode_cwd(Path::new("/home/u/a.b/c")), "-home-u-a-b-c");
    }
}
