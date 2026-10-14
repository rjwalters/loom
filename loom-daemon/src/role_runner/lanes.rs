//! Per-repository doctor lanes (#10632, part of #10630).
//!
//! #9391 made the repository the parallelism boundary with **one** instance
//! per `(repository, role)`, and `/loom:doctor` fixes one PR per run. Those
//! two caps multiply: a repository with ~60 `loom:changes-requested` PRs was
//! visited by one doctor per role-runner pass, so it drained about one PR per
//! 75 minutes per host while the host's doctor budget and token pool sat
//! partly idle.
//!
//! A repository's doctor may now hold up to
//! `clamp(ceil(repo changes debt / perRun), 1, doctorMaxPerRepo)` runs at once
//! ([`super::demand::repo_lanes`]), sized from **that repository's** ledger
//! entry, not the host total. Each run occupies a *lane* — the third element
//! of the [`InProgressGuard`] key — so it counts against the host ceiling, the
//! doctor budget and the Champion-first reservation exactly like any other
//! run ([`RoleRunGuard::admit_lane_with_demand`]). Lane `0` is the classic
//! run's key, so idle-edge runs and every other role are unchanged.
//!
//! **Each lane works a different PR.** When the repository's width is above
//! `1`, an admitted doctor run does not take the queue head itself (two runs
//! started a tick apart would race for it). On its blocking thread, before any
//! agent is spawned, it reads the shared Doctor queue (`loom-daemon pr-queue
//! --role doctor`'s own ordering, [`crate::pr_planning::fetch_queue`], which
//! already leaves out `loom:treating`, `loom:blocked` and
//! `loom:operator-only`), reserves the first row no other lane of this
//! repository holds ([`LaneAssignment`], released when the run ends), runs
//! the existing stale-verdict guard on it, and dispatches `/loom:doctor <PR>`
//! — Doctor's PR Fix Mode, which still runs the stale `loom:treating` claim
//! check before claiming. Distinct PRs on one host are therefore a guarantee
//! of the reservation, and across hosts the `loom:treating` claim arbitrates
//! as it always has.
//!
//! A lane that finds nothing assignable ends `QueueEmpty` (no agent spent). A
//! failed queue read lets lane `0` fall back to the classic unassigned run
//! (fail open, as the queue gate does) and ends an extra lane `QueueEmpty`,
//! so a listing outage can never put two unassigned doctors on one queue head.

use super::*;

/// Reads `root`'s Doctor queue as PR numbers, in the order Doctor would take
/// them. `Err` is an unreadable queue.
pub type LaneQueue = Arc<dyn Fn(&Path) -> Result<Vec<u64>, String> + Send + Sync>;

/// Runs the stale-verdict guard on one PR: `true` when Doctor may take it.
pub type VerdictCheck = Arc<dyn Fn(&Path, u64) -> bool + Send + Sync>;

/// Queue rows a lane runs the stale-verdict guard on before giving up for
/// this tick, so a queue of stale verdicts cannot turn one lane into a long
/// run of forge calls.
pub const VERDICT_ATTEMPTS: usize = 3;

/// The forge reads a lane assignment makes.
#[derive(Clone)]
pub struct LaneProbe {
    /// The Doctor queue reader.
    pub queue: LaneQueue,
    /// The stale-verdict guard.
    pub verdict: VerdictCheck,
}

impl LaneProbe {
    /// Production: the shared Doctor queue and `verdict-staleness-guard.sh`.
    #[must_use]
    pub fn forge() -> Self {
        Self {
            queue: Arc::new(|root| {
                let gh_bin = std::env::var("LOOM_GH_BIN")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .map_or_else(|| PathBuf::from(crate::gh_invocation::gh_bin()), PathBuf::from);
                crate::pr_planning::fetch_queue(root, &gh_bin, crate::pr_planning::PrRole::Doctor)
                    .map(|rows| rows.iter().filter_map(|r| r["number"].as_u64()).collect())
                    .map_err(|e| e.to_string())
            }),
            verdict: Arc::new(forge_verdict_check),
        }
    }

    /// A probe that never reads the forge: every queue read fails, so lane `0`
    /// runs unassigned and extra lanes stand down (the test-dispatcher
    /// default).
    #[must_use]
    pub fn none() -> Self {
        Self {
            queue: Arc::new(|_| Err("no lane probe configured".to_string())),
            verdict: Arc::new(|_, _| false),
        }
    }
}

/// `verdict-staleness-guard.sh <PR> --clear`, with `doctor.md`'s exit-code
/// reading: `0` (fresh) and `11` (unverifiable, kept) may be taken; `10` (no
/// verdict left), `12` (stale — the guard re-queued it for Judge) and any
/// error may not. A repository without the script installed is taken, exactly
/// as a sweep dispatches PR Fix Mode.
fn forge_verdict_check(root: &Path, pr: u64) -> bool {
    let script = root.join(".loom/scripts/verdict-staleness-guard.sh");
    if !script.is_file() {
        return true;
    }
    let output = Command::new(&script)
        .current_dir(root)
        .arg(pr.to_string())
        .arg("--clear")
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) => {
            let code = out.status.code();
            log::debug!(
                "role_runner: doctor lane stale-verdict guard for #{pr} in {} exited {code:?} \
                 (#10632)",
                root.display()
            );
            matches!(code, Some(0 | 11))
        }
        Err(e) => {
            log::debug!(
                "role_runner: doctor lane stale-verdict guard for #{pr} failed to run: {e}"
            );
            false
        }
    }
}

/// The PRs an in-flight lane holds, per repository, host-wide.
fn assigned() -> &'static Mutex<HashSet<(PathBuf, u64)>> {
    static ASSIGNED: OnceLock<Mutex<HashSet<(PathBuf, u64)>>> = OnceLock::new();
    ASSIGNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// One lane's hold on one PR; dropping it frees the PR for another lane.
#[derive(Debug)]
pub struct LaneAssignment {
    root: PathBuf,
    pr: u64,
}

impl LaneAssignment {
    /// Hold `pr` for `root`, or `None` when another lane already holds it.
    #[must_use]
    pub fn reserve(root: &Path, pr: u64) -> Option<Self> {
        let mut held = assigned().lock().unwrap_or_else(PoisonError::into_inner);
        held.insert((root.to_path_buf(), pr)).then(|| Self {
            root: root.to_path_buf(),
            pr,
        })
    }

    /// The PR this lane works.
    #[must_use]
    pub fn pr(&self) -> u64 {
        self.pr
    }
}

impl Drop for LaneAssignment {
    fn drop(&mut self) {
        let mut held = assigned().lock().unwrap_or_else(PoisonError::into_inner);
        held.remove(&(std::mem::take(&mut self.root), self.pr));
    }
}

/// What an admitted lane runs.
#[derive(Debug)]
pub enum LaneTarget {
    /// `/loom:doctor <PR>`, holding the assignment for the run.
    Pr(LaneAssignment),
    /// The classic `/loom:doctor` queue scan (lane `0` when the queue could not
    /// be read — fail open).
    Unassigned,
    /// Nothing this lane may take: end the run `QueueEmpty`.
    Nothing,
}

/// Pick the PR `lane` of `root` works (see the module doc).
#[must_use]
pub fn assign(probe: &LaneProbe, root: &Path, lane: usize) -> LaneTarget {
    let rows = match (probe.queue)(root) {
        Ok(rows) => rows,
        Err(e) => {
            log::debug!(
                "role_runner: doctor lane {lane} queue read for {} failed ({e}) — {} (#10632)",
                root.display(),
                if lane == 0 {
                    "running the classic queue scan (fail open)"
                } else {
                    "standing this extra lane down"
                }
            );
            return if lane == 0 {
                LaneTarget::Unassigned
            } else {
                LaneTarget::Nothing
            };
        }
    };
    let mut tried = 0;
    for pr in rows {
        if tried == VERDICT_ATTEMPTS {
            break;
        }
        let Some(hold) = LaneAssignment::reserve(root, pr) else {
            continue;
        };
        tried += 1;
        if (probe.verdict)(root, pr) {
            return LaneTarget::Pr(hold);
        }
    }
    LaneTarget::Nothing
}

impl RoleRunGuard {
    /// The lane this run holds (`0` for every classic run).
    #[must_use]
    pub fn lane(&self) -> usize {
        self.key.2
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/lanes.rs"]
mod tests;
