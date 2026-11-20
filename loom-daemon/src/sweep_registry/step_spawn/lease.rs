//! A non-Builder step's role-keyed lease (#11906): the pre-claim, its
//! renewal while the step runs, and its reap when the step ends.
//!
//! All three run the daemon's own `lease pr` verbs, so the lease a step holds
//! is byte-for-byte the one its prompt's `lease pr claim` / `fence` /
//! `release` read: `<!-- loom:pr-lease role=<role> host=… sweep=<step id>
//! step=<step> -->`.
//!
//! - **Pre-claim**, before the spawn, with no `--watch-pid` (no session to
//!   watch yet): claim-then-verify-order, a yield on loss. The won lease is
//!   listed in the step's lease file (`pr_lease_reap`).
//! - **Renewal**, once the child exists: `lease pr attach --watch-pid
//!   <child>`, renew-only. It re-attaches to the step's own live lease and
//!   starts the detached renewer, which lives as long as the step's session,
//!   never the daemon (#6129). A lease already released or reaped stays
//!   gone: `attach` never posts one. The issue path's renewal ran
//!   only for issue sweeps (`dispatch.rs`), while `child_env_markers` told
//!   every step it was renewed; a Builder step now takes that path, every
//!   other step this one.
//! - **Reap**, when the step ends: `lease pr reap` for every lease still
//!   listed, as the role runner does for a timed-out run (#11931), so a peer
//!   need not wait out the TTL.

use super::*;
use crate::pr_lease_reap::{self, Reaper};

/// `lease pr claim` exits (`cli/pr_lease.rs`).
const EXIT_HELD: i32 = 0;
const EXIT_UNVERIFIED: i32 = 5;
const EXIT_PEER: i32 = 4;
const EXIT_BACKSTOP: i32 = 6;
const EXIT_ACTIVE: i32 = 7;

/// The deadline for one `lease pr claim`: bounded reads and one post, each
/// with its own `gh` deadline (as `pr_lease_reap::REAP_TIMEOUT`).
const CLAIM_TIMEOUT: Duration = pr_lease_reap::REAP_TIMEOUT;

/// The `lease pr` argv (after the binary) for a step's lease: the pre-claim
/// (`claim`, no `watch_pid`), or the renewal start (`attach --watch-pid`,
/// renew-only: it never posts a lease). Pure.
#[must_use]
pub(crate) fn claim_args(
    root: &Path,
    item: u32,
    role: StepRole,
    sweep_id: &str,
    watch_pid: Option<u32>,
) -> Vec<String> {
    let verb = if watch_pid.is_some() {
        "attach"
    } else {
        "claim"
    };
    let mut args = vec![
        "lease".to_string(),
        "pr".to_string(),
        verb.to_string(),
        item.to_string(),
        "--role".to_string(),
        role.role_name().to_string(),
        "--step".to_string(),
        role.as_str().to_string(),
        "--sweep-id".to_string(),
        sweep_id.to_string(),
    ];
    if let Some(pid) = watch_pid {
        args.extend(["--watch-pid".to_string(), pid.to_string()]);
    }
    args.extend([
        "--workspace".to_string(),
        root.to_string_lossy().into_owned(),
    ]);
    args
}

/// The longest stderr tail a claim's span carries, in bytes.
const STDERR_TAIL_MAX: usize = 256;
/// How long to wait for the stderr reader after the claim ended: a
/// grandchild may still hold the pipe open.
const STDERR_GRACE: Duration = Duration::from_millis(500);

/// How one bounded command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimRun {
    Exited(i32),
    /// Past its deadline, killed.
    TimedOut,
    /// The binary could not be started.
    SpawnFailed,
    /// Ended by a signal (no exit code).
    Signaled,
}

impl ClaimRun {
    /// The closed `loom.dispatch.claim_outcome` token.
    fn token(&self) -> &'static str {
        match self {
            Self::Exited(_) => "exited",
            Self::TimedOut => "timed_out",
            Self::SpawnFailed => "spawn_failed",
            Self::Signaled => "signaled",
        }
    }

    /// The exit code, when the command exited.
    fn code(&self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(*code),
            _ => None,
        }
    }
}

/// What a pre-claim's run left behind, for the `loom.dispatch.step` span:
/// why a `claim_unverified` was (or why a won claim took as long as it did).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimFacts {
    /// `exited`, `timed_out`, `spawn_failed`, `signaled` or `no_binary`.
    pub outcome: &'static str,
    pub exit_code: Option<i32>,
    pub claim_ms: u64,
    /// The last line of the claim's stderr, sanitised and bounded.
    pub stderr_tail: Option<String>,
    /// What a refused Builder claim's rollback did (#12822): `none`,
    /// `lease_yielded`, `label_restored`, `both` or `failed`. `None` when no
    /// rollback ran (a role-keyed claim, which reaps its own leases).
    pub rollback: Option<&'static str>,
}

/// What a pre-claim's exit says. Pure.
pub(crate) fn claim_verdict(run: &ClaimRun) -> std::result::Result<(), StepRefusal> {
    match run {
        ClaimRun::Exited(EXIT_HELD) => Ok(()),
        ClaimRun::Exited(EXIT_PEER) => {
            Err(StepRefusal::new("claim_lost", "a peer holds the live lease"))
        }
        ClaimRun::Exited(EXIT_BACKSTOP) => {
            Err(StepRefusal::new("backstop_hold", "the Judge no-verdict backstop holds this PR"))
        }
        // #12105: another actor (a live lease of another kind, or a recent
        // push by another login) is working the PR; a Doctor stands down.
        ClaimRun::Exited(EXIT_ACTIVE) => Err(StepRefusal::new(
            "peer_active",
            "another actor is active on this PR (a live lease or a recent push)",
        )),
        ClaimRun::Exited(code) => {
            Err(StepRefusal::new("claim_unverified", format!("`lease pr claim` exited {code}")))
        }
        ClaimRun::TimedOut => {
            Err(StepRefusal::new("claim_unverified", "`lease pr claim` timed out"))
        }
        ClaimRun::SpawnFailed => {
            Err(StepRefusal::new("claim_unverified", "`lease pr claim` could not start"))
        }
        ClaimRun::Signaled => {
            Err(StepRefusal::new("claim_unverified", "`lease pr claim` ended by a signal"))
        }
    }
}

/// The last non-empty line of `raw`, control characters stripped, cut to
/// [`STDERR_TAIL_MAX`] bytes on a char boundary. Pure.
fn stderr_tail(raw: &str) -> Option<String> {
    let line = raw.lines().rev().find(|l| !l.trim().is_empty())?;
    let clean: String = line.trim().chars().filter(|c| !c.is_control()).collect();
    let mut end = clean.len().min(STDERR_TAIL_MAX);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    let tail = clean[..end].to_string();
    (!tail.is_empty()).then_some(tail)
}

/// Run `cmd` to completion within `limit`; how it ended, how long it took,
/// and the tail of its stderr plus its (bounded) stdout when `cmd` pipes
/// them. The stdout is the claim child's typed channel
/// (`writer_limit::parse_handoff`): vendor output never reaches it.
fn run_bounded(
    cmd: &mut Command,
    limit: Duration,
) -> (ClaimRun, Duration, Option<String>, Option<String>) {
    let start = Instant::now();
    let Ok(mut child) = cmd.spawn() else {
        return (ClaimRun::SpawnFailed, start.elapsed(), None, None);
    };
    // #14100: a `lease pr` child is the daemon's own binary; attribute its pid.
    let tracked = crate::self_spawn_log::track("step_lease", cmd, child.id());
    // Drained on threads, so a chatty child cannot fill a pipe and stall.
    fn drain<R: std::io::Read + Send + 'static>(
        stream: R,
    ) -> Option<std::sync::mpsc::Receiver<String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::Read::take(stream, 65_536), &mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        Some(rx)
    }
    let stderr_reader = child.stderr.take().and_then(drain);
    let stdout_reader = child.stdout.take().and_then(drain);
    let run = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracked.finish(status);
                break status.code().map_or(ClaimRun::Signaled, ClaimRun::Exited);
            }
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracked.finish("timed out");
                break ClaimRun::TimedOut;
            }
        }
    };
    let elapsed = start.elapsed();
    let tail = stderr_reader
        .and_then(|rx| rx.recv_timeout(STDERR_GRACE).ok())
        .and_then(|raw| stderr_tail(&raw));
    let stdout = stdout_reader.and_then(|rx| rx.recv_timeout(STDERR_GRACE).ok());
    (run, elapsed, tail, stdout)
}

impl SweepRegistry {
    /// A `loom-daemon lease pr …` command for this workspace, its won leases
    /// listed in `sweep_id`'s lease file. `None` without a daemon binary.
    fn lease_command(&self, sweep_id: &str, args: &[String], pipe_stderr: bool) -> Option<Command> {
        let bin = match crate::daemon_bin_resolve::resolve_daemon_bin() {
            Ok(bin) => bin,
            Err(e) => {
                log::warn!("sweep_registry: no loom-daemon binary for a step lease ({e})");
                return None;
            }
        };
        let root = &self.config.workspace_root;
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(root)
            .env(pr_lease_reap::LEASE_FILE_ENV, pr_lease_reap::path_for(root, sweep_id))
            .stdin(Stdio::null())
            // The claim's stdout is its typed handoff channel (#13746); on a
            // won claim it stays the `host sweep` line, on a stand-down the
            // bounded writer facts. Piped for every verb; nobody reads the
            // renewal's.
            .stdout(Stdio::piped())
            .stderr(if pipe_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        if let Some(gh) = &self.config.gh_bin {
            cmd.env("LOOM_GH_BIN", gh);
        }
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        Some(cmd)
    }

    /// B1: pre-take a non-Builder step's role-keyed lease, and only that.
    pub(super) fn pre_claim_step_lease(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> std::result::Result<ClaimFacts, StepRefusal> {
        run_step_claim(self.step_claim_command(item, role, sweep_id))
    }

    /// The pre-claim's `lease pr claim` command, built under the lock and
    /// run by [`run_step_claim`] with it released (#11909).
    pub(super) fn step_claim_command(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> Option<Command> {
        let root = &self.config.workspace_root;
        let _ = std::fs::remove_file(pr_lease_reap::path_for(root, sweep_id));
        let args = claim_args(root, item, role, sweep_id, None);
        self.lease_command(sweep_id, &args, true)
    }

    /// Start the renewal of a just-spawned step's lease, on a detached
    /// thread (the claim's forge reads must not hold the registry lock). A
    /// Builder step's issue lease takes the issue sweep's renewal loop.
    pub(super) fn start_step_lease_renewal(
        &self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
        pid: u32,
        log_path: &Path,
    ) {
        if self.config.skip_label_flip {
            return;
        }
        if role == StepRole::Builder {
            drop(self.start_lease_renewal_loop(item, sweep_id, pid, log_path));
            return;
        }
        let args = claim_args(&self.config.workspace_root, item, role, sweep_id, Some(pid));
        let Some(mut cmd) = self.lease_command(sweep_id, &args, false) else {
            return;
        };
        let id = sweep_id.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("step-lease-renew-{item}"))
            .spawn(move || match run_bounded(&mut cmd, CLAIM_TIMEOUT).0 {
                ClaimRun::Exited(EXIT_HELD) => {}
                run => log::warn!(
                    "sweep_registry: {id}: lease renewal did not start ({run:?}); the lease \
                     ages out at the TTL"
                ),
            });
        if let Err(e) = spawned {
            log::warn!("sweep_registry: {sweep_id}: no lease-renewal thread ({e})");
        }
    }

    /// Reap every PR lease the step `sweep_id` still lists (#11931's reap),
    /// on a detached thread, recording `reason`. The lease file is removed
    /// afterwards. A no-op when the step listed none.
    pub(crate) fn reap_step_leases(&self, sweep_id: &str, reason: &'static str) {
        let reaper = Reaper::existing(&self.config.workspace_root, sweep_id);
        if reaper.held().is_empty() {
            return;
        }
        let id = sweep_id.to_string();
        let spawned = std::thread::Builder::new()
            .name("step-lease-reap".to_string())
            .spawn(move || {
                let reaped = reaper.reap_with_daemon(reason);
                log::info!("sweep_registry: {id}: reaped {reaped} lease(s) ({reason}, #11906)");
            });
        if let Err(e) = spawned {
            log::warn!("sweep_registry: {sweep_id}: no lease-reap thread ({e}); leases age out");
        }
    }
}

/// Run a pre-claim command ([`SweepRegistry::step_claim_command`]); no
/// registry state is read, so the caller holds no lock. The facts of the
/// run, won or refused (#12183). A confirmed writer rate limit the child
/// handed over (`writer_limit::parse_handoff` of its stdout, the one typed
/// channel vendor output cannot reach) is the distinct
/// `writer_rate_limited` refusal (#13746); every other way the claim fails
/// stays `claim_unverified`.
pub(super) fn run_step_claim(cmd: Option<Command>) -> std::result::Result<ClaimFacts, StepRefusal> {
    let Some(mut cmd) = cmd else {
        let mut refusal = StepRefusal::new("claim_unverified", "no loom-daemon binary");
        refusal.claim = Some(Box::new(ClaimFacts {
            outcome: "no_binary",
            exit_code: None,
            claim_ms: 0,
            stderr_tail: None,
            rollback: None,
        }));
        return Err(refusal);
    };
    let (run, elapsed, tail, stdout) = run_bounded(&mut cmd, CLAIM_TIMEOUT);
    let facts = ClaimFacts {
        outcome: run.token(),
        exit_code: run.code(),
        claim_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        stderr_tail: tail,
        rollback: None,
    };
    // The typed handoff only ever upgrades the child's own unverified
    // exit: an EXIT_HELD (or any other) run never reads one.
    let writer_limited = if run.code() == Some(EXIT_UNVERIFIED) {
        stdout
            .as_deref()
            .and_then(crate::writer_limit::parse_handoff)
    } else {
        None
    };
    match claim_verdict(&run) {
        Ok(()) => Ok(facts),
        Err(mut refusal) => {
            if let Some(limited) = writer_limited {
                let reset = limited
                    .reset_utc()
                    .map_or_else(|| "reset unconfirmed".to_string(), |at| at.to_string());
                refusal = StepRefusal::new(
                    crate::writer_limit::OUTCOME,
                    format!("the writer is rate-limited ({}): {}", limited.stage.as_str(), reset),
                );
                refusal.writer_limit = Some(Box::new(limited));
            } else if let Some(tail) = &facts.stderr_tail {
                refusal.detail = format!("{} (stderr: {tail})", refusal.detail);
            }
            refusal.claim = Some(Box::new(facts));
            Err(refusal)
        }
    }
}

#[cfg(test)]
mod facts_tests {
    use super::*;

    #[test]
    fn the_stderr_tail_is_the_last_line_clean_and_bounded() {
        assert_eq!(stderr_tail("a\nboom: no gh\n\n").as_deref(), Some("boom: no gh"));
        assert_eq!(stderr_tail("x\u{1b}[31mred\u{7}").as_deref(), Some("x[31mred"));
        assert_eq!(stderr_tail("  \n"), None);
        let long = "é".repeat(300);
        let tail = stderr_tail(&long).unwrap();
        assert!(tail.len() <= STDERR_TAIL_MAX);
        assert!(tail.chars().all(|c| c == 'é'));
    }

    #[test]
    fn each_way_a_claim_can_end_has_its_outcome() {
        let sh = |script: &str| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script])
                .stderr(Stdio::piped())
                .stdout(Stdio::piped());
            run_bounded(&mut cmd, Duration::from_secs(5))
        };
        let (run, _, tail, stdout) = sh("echo cause >&2; exit 5");
        assert_eq!((run.token(), run.code()), ("exited", Some(5)));
        assert_eq!(tail.as_deref(), Some("cause"));
        assert_eq!(stdout.as_deref(), Some(""));
        let (run, _, _, _) = sh("kill -9 $$");
        assert_eq!((run.token(), run.code()), ("signaled", None));
        let (run, _, _, _) =
            run_bounded(&mut Command::new("/nonexistent/loom-claim"), CLAIM_TIMEOUT);
        assert_eq!(run.token(), "spawn_failed");
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 5"]);
        let (run, elapsed, _, _) = run_bounded(&mut slow, Duration::from_millis(300));
        assert_eq!(run.token(), "timed_out");
        assert!(elapsed < Duration::from_secs(3));
    }

    /// A confirmed writer rate limit the child handed over on stdout is the
    /// distinct refusal (#13746); vendor stderr shaped like a marker never
    /// is. The stub stands in for `lease pr claim` (both pipes set, as
    /// [`SweepRegistry::step_claim_command`] builds it).
    #[test]
    fn a_typed_handoff_upgrades_only_the_unverified_exit() {
        let handoff = crate::writer_limit::WriterLimited::classify(
            crate::writer_limit::WriterStage::LeasePost,
            None,
            "gh: API rate limit exceeded for installation ID 151241341 (HTTP 403)",
        )
        .unwrap();
        let claim = |script: &str| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script])
                .stderr(Stdio::piped())
                .stdout(Stdio::piped());
            run_step_claim(Some(cmd))
        };
        // Exit 5 + typed handoff: the writer refusal.
        let line = handoff.handoff_line();
        let e = claim(&format!("echo boom >&2; echo '{line}'; exit 5")).unwrap_err();
        assert_eq!(e.reason, crate::writer_limit::OUTCOME);
        assert!(e.writer_limit.is_some());
        assert_eq!(
            e.claim.as_ref().and_then(|c| c.stderr_tail.as_deref()),
            Some("boom"),
            "the stderr tail stays the diagnostic"
        );
        // Exit 5 with vendor stderr shaped like a marker: still unverified.
        let e = claim("echo 'loom-claim-writer-limited {\"stage\":\"comment_post\"}' >&2; exit 5")
            .unwrap_err();
        assert_eq!(e.reason, "claim_unverified");
        assert!(e.writer_limit.is_none());
        // A handoff on a won claim is ignored.
        let line = handoff.handoff_line();
        assert!(claim(&format!("echo '{line}'; exit 0")).is_ok());
        // Exit 4 (a peer) with a handoff is still a peer loss.
        let line = handoff.handoff_line();
        let e = claim(&format!("echo '{line}'; exit 4")).unwrap_err();
        assert_eq!(e.reason, "claim_lost");
    }
}
