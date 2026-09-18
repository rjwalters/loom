//! `loom-daemon lease ensure` — give an in-session Task-tool builder's
//! `loom:building` claim the same liveness lease a daemon-dispatched sweep
//! already publishes (#8193).
//!
//! ## Root cause this closes
//!
//! An in-session builder (an operator running `/loom:builder` directly, or a
//! Builder spawned one level deep as a Task subagent) claims `loom:building`
//! but publishes no `<!-- loom:lease host=... sweep=... -->` liveness record,
//! because `SweepRegistry::dispatch` never ran for it — that write-on-dispatch
//! path (`write_lease_comment`, #6179) only fires for a daemon-owned claim, and
//! the in-session publish step lives in the SWEEP orchestrator's prompt
//! (`sweep-wave-lifecycle.md` Step 1a/1b), not the builder's.
//!
//! `claim_reconciliation`'s Phase-2 reclaim gate (#6286) then sees
//! `lease_evidence=absent` and flips a claim that is still actively being
//! worked back to `loom:issue`. Observed 2026-09-17: three such reclaims
//! mid-build on a single six-builder wave.
//!
//! ## Why this lives in `loom-daemon` rather than in `worktree.sh`
//!
//! `defaults/scripts/worktree.sh` — the one call site every builder runs
//! immediately after claiming — is a shell-budget `contract` file, i.e. part of
//! the **portable** pool epic #7810 exists to retire. `check_against_rev`
//! refuses portable growth *unconditionally*: unlike floor growth, it has no
//! `Shell-Budget-Growth:` override (see `shell_budget.rs`, "Portable growth is
//! NOT overridable, deliberately"). So the ADR-0018 shape is not merely
//! preferred here, it is the only admissible one — the behavior goes into this
//! subcommand and `worktree.sh` keeps a two-line reach into it.
//!
//! #7672 is the precedent for putting it in code at all rather than in prose: a
//! prose-mandated lease step was skipped by exactly one session and cost ~2.5h
//! of fleet claim/yield thrash, which is why the sweep's own renewal `start`
//! moved into dispatch code.
//!
//! ## What this does NOT reimplement
//!
//! No new lease format and no new publish/renew primitive — both already exist
//! (`sweep-lease-publish.sh` #6320, `sweep-lease-renew.sh` #6180) and are
//! invoked here unmodified. This subcommand is purely the missing CALL SITE,
//! mirroring what `sweep-wave-lifecycle.md`'s Step 1a/1b already does for the
//! sweep-orchestrator path.
//!
//! ## Why it does not first check that the issue carries `loom:building`
//!
//! Deliberate, and consistent with the path this mirrors:
//! `sweep-lease-publish.sh`'s own documented semantics are
//! publish-at-pre-flight, not publish-at-claim — "the question a reader asks is
//! 'is a live worker on this issue right now?', and the pre-flight instant is
//! when that becomes true". Creating an issue worktree IS that instant. A label
//! read would also add a `gh` call to every worktree creation, and publication
//! is already bounded three ways: it is a no-op while this sweep's own lease is
//! fresh, it refuses outright when a *different* host holds a fresh lease
//! (exit 4), and it only ever runs inside an agent session (see
//! [`SessionEnv`]).

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Absolute cap on the renewal loop's lifetime, in seconds (4h).
///
/// The watched pid is the *session's* own harness process, which for an
/// operator's interactive session outlives the subagent that did the build — so
/// an uncapped loop can keep a finished builder's claim looking fresh
/// indefinitely (#8193 finding 3). `sweep-lease-renew.sh`'s own default is 24h,
/// which is the right backstop for a whole sweep but far too loose for one
/// builder.
const DEFAULT_MAX_AGE_SECS: u64 = 14_400;

/// Set by `loom-daemon` on every sweep child it dispatches, to the issue number
/// it has already published + started renewal for (#7672).
const DISPATCHED_MARKER: &str = "LOOM_SWEEP_LEASE_RENEW_DISPATCHED";

/// Environment markers that mean "a durable agent session is hosting this
/// call". Any one of them suffices.
const SESSION_MARKERS: &[&str] = &[
    "CLAUDE_PID",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "LOOM_TERMINAL_ID",
];

#[derive(clap::Subcommand)]
pub(crate) enum LeaseCommand {
    /// Publish a lease for `<issue>` (unless the daemon already did) and start
    /// a bounded renewal loop.
    ///
    /// Always exits `0` — this is best-effort, fail-open infrastructure
    /// mirroring #6179's own contract. The `loom:building` label remains the
    /// authoritative claim; a lease only improves the evidence, so nothing here
    /// may block or fail a builder's actual work.
    Ensure(LeaseEnsureArgs),
}

impl LeaseCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            LeaseCommand::Ensure(args) => args.run(),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct LeaseEnsureArgs {
    /// The issue whose `loom:building` claim needs a lease.
    #[arg(value_name = "ISSUE")]
    pub(crate) issue: u64,

    /// The durable pid the renewal loop watches for its whole lifetime.
    ///
    /// MUST be computed by the CALLER as `${CLAUDE_PID:-$PPID}` — never `$$`,
    /// which is the one-shot tool-call subshell that exits the instant the call
    /// returns, so the loop would self-terminate on its first wake-up and the
    /// lease would age out anyway (#8193 finding 1). This subcommand only
    /// forwards the value to `sweep-lease-renew.sh start --watch-pid`, which
    /// pairs it with a start-time identity token so a recycled pid number reads
    /// as dead (#7825).
    #[arg(long, value_name = "PID")]
    pub(crate) watch_pid: u32,

    /// Absolute cap on the renewal loop's lifetime, in seconds. `0` =
    /// unbounded, which is not appropriate here: the watched pid is the
    /// session's own harness process, which outlives the subagent that did the
    /// build, so an uncapped loop can keep a finished builder's claim looking
    /// fresh indefinitely (#8193 finding 3).
    #[arg(long, default_value_t = DEFAULT_MAX_AGE_SECS)]
    pub(crate) max_age: u64,

    /// Publish even without an agent-session marker in the environment.
    ///
    /// The default refusal is what keeps `worktree.sh`'s unconditional call
    /// site from posting lease comments out of a plain shell — a CI job, a
    /// shell test suite, or an operator poking at a worktree by hand. A runtime
    /// that exports none of `CLAUDE_PID` / `CLAUDECODE` /
    /// `CLAUDE_CODE_ENTRYPOINT` / `LOOM_TERMINAL_ID`, but genuinely is a
    /// long-lived agent session, can opt back in with this.
    #[arg(long)]
    pub(crate) force: bool,

    /// Repo checkout to operate in. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub(crate) workspace: String,
}

/// The parts of the process environment this decision reads, captured up front
/// so the decision itself is testable without mutating global env state.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionEnv {
    /// `LOOM_SWEEP_LEASE_RENEW_DISPATCHED`, when non-empty.
    pub(crate) dispatched_issue: Option<String>,
    /// Whether any of [`SESSION_MARKERS`] is set and non-empty.
    pub(crate) session_present: bool,
}

impl SessionEnv {
    fn from_process() -> Self {
        let non_empty = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            dispatched_issue: non_empty(DISPATCHED_MARKER),
            session_present: SESSION_MARKERS.iter().any(|k| non_empty(k).is_some()),
        }
    }
}

/// What one `lease ensure` run did. Every variant is a success from the
/// caller's point of view — this is the diagnostic, not an error channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The daemon already published + is renewing this exact issue (#7672).
    AlreadyDispatched,
    /// No agent session is hosting this call, and `--force` was not given.
    NoAgentSession,
    /// `--workspace` is not inside a Loom checkout.
    NoRepoRoot(String),
    /// The publish/renew scripts are not present under the resolved root.
    ScriptsMissing(PathBuf),
    /// `sweep-lease-publish.sh` could not be run at all.
    PublishUnavailable(String),
    /// Publish declined (exit 4: a live peer holds it) or failed (exit 2).
    PublishDeclined(Option<i32>),
    /// Publish succeeded but printed an identity line this cannot use.
    PublishUnparseable(String),
    /// Published (or already held) and the renewal loop is running.
    Renewing {
        host: String,
        sweep_id: String,
        loop_pid: Option<String>,
    },
    /// Published, but the renewal loop did not start. The lease is real and
    /// will simply age out at the TTL instead of being kept fresh.
    RenewalFailed(Option<i32>),
}

impl Outcome {
    /// The one stderr line this subcommand prints. Every path says something:
    /// a lease mechanism that is silent when it declines is exactly how #8193
    /// went unnoticed for as long as it did.
    fn describe(&self, issue: u64) -> String {
        match self {
            Self::AlreadyDispatched => format!(
                "issue #{issue} is daemon-dispatched ({DISPATCHED_MARKER} names it) — publish + \
                 renewal are already running for it; nothing to do"
            ),
            Self::NoAgentSession => format!(
                "no agent session in the environment (none of {}) — not publishing a lease for \
                 issue #{issue}. A lease is only meaningful while a durable session process is \
                 alive to be watched; pass --force to publish anyway.",
                SESSION_MARKERS.join(", ")
            ),
            Self::NoRepoRoot(why) => {
                format!("could not resolve a Loom repo root: {why} — skipping issue #{issue}")
            }
            Self::ScriptsMissing(root) => format!(
                "sweep-lease-publish.sh / sweep-lease-renew.sh not found under {} — skipping \
                 issue #{issue} (best-effort, mirrors #6179's fail-open contract)",
                root.display()
            ),
            Self::PublishUnavailable(why) => format!(
                "could not run sweep-lease-publish.sh: {why} — proceeding without a lease on \
                 issue #{issue}"
            ),
            Self::PublishDeclined(code) => format!(
                "publish for issue #{issue} exited {code:?} (4 = a live peer host holds a fresh \
                 lease; 2 = the publish gh call failed) — proceeding without a lease"
            ),
            Self::PublishUnparseable(line) => format!(
                "could not parse '<host> <sweep-id>' out of sweep-lease-publish.sh's output for \
                 issue #{issue}: {line:?} — proceeding without renewal"
            ),
            Self::Renewing {
                host,
                sweep_id,
                loop_pid,
            } => format!(
                "issue #{issue}: lease published/held (host={host} sweep={sweep_id}) and renewal \
                 loop {} is running",
                loop_pid.as_deref().unwrap_or("(pid unknown)")
            ),
            Self::RenewalFailed(code) => format!(
                "issue #{issue}: lease published, but sweep-lease-renew.sh start exited {code:?} \
                 — the lease will age out at the TTL rather than be kept fresh"
            ),
        }
    }
}

impl LeaseEnsureArgs {
    pub(crate) fn run(self) -> Result<()> {
        let outcome = self.ensure(&SessionEnv::from_process());
        eprintln!("lease ensure: {}", outcome.describe(self.issue));
        Ok(())
    }

    /// The whole decision, with the environment injected so it is testable.
    pub(crate) fn ensure(&self, env: &SessionEnv) -> Outcome {
        // #8193 finding 2: when `LOOM_SWEEP_LEASE_RENEW_DISPATCHED` already
        // names THIS issue, the daemon published the lease
        // (`SweepRegistry::dispatch` -> `write_lease_comment`, #6179) and
        // started renewal against this same child process (#7672) before
        // spawning it. Publishing again is safe — readers take the freshest
        // record — but leaves a duplicate lease comment on the issue, so no-op.
        if env
            .dispatched_issue
            .as_deref()
            .is_some_and(|d| d == self.issue.to_string())
        {
            return Outcome::AlreadyDispatched;
        }

        if !env.session_present && !self.force {
            return Outcome::NoAgentSession;
        }

        let root = match loom_daemon::repo_root::resolve_repo_root(&self.workspace) {
            Ok(r) => r,
            Err(e) => return Outcome::NoRepoRoot(e.to_string()),
        };
        let publish_script = root.join(".loom/scripts/sweep-lease-publish.sh");
        let renew_script = root.join(".loom/scripts/sweep-lease-renew.sh");
        if !publish_script.is_file() || !renew_script.is_file() {
            return Outcome::ScriptsMissing(root);
        }

        match self.publish(&publish_script, &root) {
            Ok((host, sweep_id)) => self.start_renewal(&renew_script, &root, &host, &sweep_id),
            Err(outcome) => outcome,
        }
    }

    /// Publish the lease, returning the `(host, sweep-id)` identity to thread
    /// into renewal.
    ///
    /// `sweep-lease-publish.sh` prints exactly that pair on stdout on exit 0
    /// and routes every diagnostic (`OK:`, `NOTE:`, `SKIP:`, `WARN:`) to
    /// stderr, so the parse below is the documented contract rather than a
    /// guess. Every failure is folded into an `Outcome` and swallowed: a lease
    /// is evidence, never a precondition (#6179's fail-open contract).
    fn publish(&self, publish_script: &Path, root: &Path) -> Result<(String, String), Outcome> {
        let output = Command::new(publish_script)
            .arg("publish")
            .arg(self.issue.to_string())
            .current_dir(root)
            .output()
            .map_err(|e| Outcome::PublishUnavailable(e.to_string()))?;

        if !output.status.success() {
            return Err(Outcome::PublishDeclined(output.status.code()));
        }

        let ident = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let mut parts = ident.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(host), Some(sweep_id)) => Ok((host.to_string(), sweep_id.to_string())),
            _ => Err(Outcome::PublishUnparseable(ident)),
        }
    }

    /// Start the bounded renewal loop for the identity `publish` resolved.
    ///
    /// # Why stderr is `Stdio::null()` rather than captured
    ///
    /// `sweep-lease-renew.sh start` runs `exec 9>&2` before forking its
    /// detached loop, deliberately: fd 9 is the loop's private channel for
    /// reporting a failing renewal cycle (#6541). The detached loop therefore
    /// inherits a duplicate of whatever fd 2 was at `start` time and holds it
    /// open for the loop's ENTIRE lifetime. Capturing stderr into a pipe would
    /// hand it the write end of that pipe, and `Command::output()` — which
    /// reads until every pipe closes — would block here for up to `--max-age`
    /// seconds. Nulling stderr points fd 9 at `/dev/null`, exactly as the
    /// documented shell call site's `> /dev/null 2>&1` does. Stdout is safe to
    /// pipe: the loop redirects its own to `/dev/null`, so only `start` itself
    /// holds it, and it prints the loop pid there.
    fn start_renewal(
        &self,
        renew_script: &Path,
        root: &Path,
        host: &str,
        sweep_id: &str,
    ) -> Outcome {
        let result = Command::new(renew_script)
            .arg("start")
            .arg(self.issue.to_string())
            .arg("--watch-pid")
            .arg(self.watch_pid.to_string())
            .arg("--max-age")
            .arg(self.max_age.to_string())
            .arg("--host")
            .arg(host)
            .arg("--sweep-id")
            .arg(sweep_id)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        match result {
            Ok(o) if o.status.success() => {
                let pid = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Outcome::Renewing {
                    host: host.to_string(),
                    sweep_id: sweep_id.to_string(),
                    loop_pid: (!pid.is_empty()).then_some(pid),
                }
            }
            Ok(o) => Outcome::RenewalFailed(o.status.code()),
            Err(_) => Outcome::RenewalFailed(None),
        }
    }
}

#[cfg(test)]
mod tests;
