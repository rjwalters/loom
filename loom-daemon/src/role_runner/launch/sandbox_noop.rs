//! Turn an exit-0 **Codex** role tick that ran nothing, because the runtime's
//! sandbox refused every tool call, into a [`RoleTickOutcome::Failure`], and
//! route the next ticks around it (issue #10003; cause #9979).
//!
//! Inside a session container that cannot create user namespaces, Codex's
//! bubblewrap sandbox refuses every shell command (`bwrap: …`). The agent
//! says so in prose and `codex exec` exits 0. Before this module that tick
//! was a `Success`. So the daemon's success tally, `role_tick.outcome`
//! telemetry and the dashboard all read healthy, while curator, judge and
//! champion did nothing for days. The preference walk also kept choosing
//! Codex, because nothing about the account pool had changed.
//!
//! **Where the verdict comes from.** The rule is
//! [`crate::codex_sandbox_noop::scan`], which reads Codex's own exec framing
//! and never agent prose. `spawn-codex.sh` asks for it through
//! `loom-daemon codex-sandbox-noop` on every exit-0 session. On a no-op it
//! writes the tick's terminal record as `category=SANDBOX_UNAVAILABLE`, plus
//! a `# LOOM_RUNTIME_NOOP …` line naming the shape and counts. This module
//! reads **this tick's own** region of the role log, everything after its
//! `tick_anchor` (the same scoping `provider_health_feedback` and
//! `toolless_launch` use). A stale record from an earlier tick in the
//! append-only log can therefore never fail this one. The record decides.
//! When the record says `SUCCESS`, the same scan runs over the region
//! in-process, because a workspace whose installed adapter predates the
//! verdict still says `SUCCESS` for a no-op. The child's stderr is that
//! region, so the scan sees the same framing the adapter would have.
//!
//! **What follows from a no-op:**
//!
//! 1. The tick is a `Failure` with a stable, greppable reason
//!    ([`REASON_PREFIX`]). It is counted, logged and escalated like any other
//!    failed tick, never as a success. This is the same reasoning as
//!    `toolless_launch` (#8448).
//! 2. The host-wide `runtime_preference::sandbox_hold` is armed for the
//!    runtime. While it is live the preference walk passes the tap over, so
//!    the next tick in any workspace falls through to the next tap
//!    (`rolePreference: ["codex","claude"]` → Claude). Operator pins are not
//!    affected; they never reach the walk.
//! 3. Account health is untouched: `SANDBOX_UNAVAILABLE` records no hold and
//!    no `last_success` (`tokens_pool::health`).
//!
//! A Codex tick whose terminal record is `SUCCESS` **and** whose region shows
//! a shell command that ran to a ` succeeded in` result proves the sandbox
//! runs commands again, so it clears the hold at once instead of waiting for
//! it to age out. A `SUCCESS` that matches no no-op shape but shows no command
//! succeeding either is no opinion and leaves the hold alone. robb-studio had
//! two such ticks on 2026-10-03, where every shell call failed on a read-only
//! sandbox registry lock with no exec echoed, and they must not clear a hold a
//! real no-op armed.

use super::super::read_role_log;
use crate::tokens_pool::TerminalClassification;
use std::path::Path;

/// Leading text of every no-op failure detail, so the daemon log, the
/// `role_tick.outcome` record and `fleet-check` can count these ticks
/// separately from every other failure.
pub(crate) const REASON_PREFIX: &str = "runtime sandbox unavailable";

/// The adapter's no-op line (`spawn-codex.sh`, #10003).
const NOOP_MARKER: &str = "# LOOM_RUNTIME_NOOP ";

/// What this tick's own region of the role log says about the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The runtime's sandbox refused every tool call; carries the failure
    /// detail that replaces `Success`.
    NoOp(String),
    /// The tick's record is `SUCCESS` and its region shows a shell command
    /// that ran to a ` succeeded in` result. Positive evidence only: a
    /// region that merely matches no no-op shape is [`Self::NoOpinion`].
    Ran,
    /// Not a Codex tick, no admission, no readable/unique record, or any
    /// other category: no opinion, so nothing changes.
    NoOpinion,
}

/// Pure core of [`detect`]: classify `contents` (the whole role log) for the
/// tick whose header carries `tick_anchor`.
pub(super) fn verdict_in(contents: &str, runtime: &str, tick_anchor: &str) -> Verdict {
    if runtime != "codex" || tick_anchor.is_empty() {
        return Verdict::NoOpinion;
    }
    let Some(result) = crate::sweep_registry::parse_terminal_result_after(contents, tick_anchor)
    else {
        return Verdict::NoOpinion;
    };
    // `parse_terminal_result_after` already proved the anchor is present.
    let region = contents.rfind(tick_anchor).map_or("", |at| &contents[at..]);
    match result.category {
        TerminalClassification::SandboxUnavailable => {
            let shape = region
                .lines()
                .rev()
                .find_map(|line| line.strip_prefix(NOOP_MARKER))
                .map_or_else(
                    || "no detail line".to_string(),
                    // Keep the `shape=… execs=…` tail; the leading
                    // `runtime=… reason=…` repeats what the sentence says.
                    |rest| {
                        let rest = rest.find("shape=").map_or(rest, |at| &rest[at..]);
                        super::super::clean_and_cap_detail(rest.trim())
                    },
                );
            Verdict::NoOp(describe(runtime, &shape))
        }
        TerminalClassification::Success => match crate::codex_sandbox_noop::scan(region) {
            Some(noop) => Verdict::NoOp(describe(runtime, &noop.to_string())),
            // Only positive evidence clears the hold. A SUCCESS whose region
            // matches no no-op shape but shows no command running either
            // (every call failed some other way without an echoed exec
            // result) is no opinion, so the hold ages out as designed.
            None if crate::codex_sandbox_noop::ran_a_command(region) => Verdict::Ran,
            None => Verdict::NoOpinion,
        },
        _ => Verdict::NoOpinion,
    }
}

fn describe(runtime: &str, shape: &str) -> String {
    format!(
        "{REASON_PREFIX} ({runtime}): the session exited 0 but its sandbox refused every tool \
         call, so nothing ran ({shape}) — reported as a failed tick, not a success; {runtime} \
         is passed over by the preference walk until the hold ages out (#10003, cause #9979)"
    )
}

/// Filesystem wrapper over [`verdict_in`] for a just-exited-0 tick. An
/// unreadable log is no opinion, never a failure.
pub(super) fn detect(
    log_path: &Path,
    admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    tick_anchor: &str,
) -> Verdict {
    let Some(admission) = admission else {
        return Verdict::NoOpinion;
    };
    // Cheap pre-check before reading a potentially large role log.
    if admission.runtime != "codex" {
        return Verdict::NoOpinion;
    }
    verdict_in(&read_role_log(log_path), &admission.runtime, tick_anchor)
}

/// Apply a verdict's side effect on the host-wide sandbox hold and return the
/// failure detail, if the tick must not be reported as a success.
pub(super) fn apply(verdict: Verdict, runtime: &str, now: u64) -> Option<String> {
    match verdict {
        Verdict::NoOp(detail) => {
            crate::runtime_preference::sandbox_hold::arm(runtime, &detail_for_hold(&detail), now);
            Some(detail)
        }
        Verdict::Ran => {
            if crate::runtime_preference::sandbox_hold::clear(runtime) {
                log::info!(
                    "role_runner: {runtime} tick ran a tool call — sandbox hold cleared (#10003)"
                );
            }
            None
        }
        Verdict::NoOpinion => None,
    }
}

/// The short form the hold carries into the preference marker: just the
/// adapter's `shape=… execs=…` detail, not the whole failure sentence.
fn detail_for_hold(detail: &str) -> String {
    detail
        .split_once("nothing ran (")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map_or_else(
            || "sandbox refused every tool call".to_string(),
            |(shape, _)| shape.to_string(),
        )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ANCHOR: &str = "2026-10-02T21:04:42.924756Z";

    fn tick(anchor: &str, body: &str) -> String {
        format!(
            "\n==== loom-daemon role_runner: {anchor} role=curator model=<runtime CLI default> ====\n\
             # LOOM_ACCOUNT name=agent-1\n{body}"
        )
    }

    const NOOP_BODY: &str = "exec\n/bin/bash -lc 'cat SKILL.md' in /w\n exited 1 in 0ms:\n\
         bwrap: No permissions to create a new namespace\n\
         # LOOM_RUNTIME_NOOP runtime=codex reason=sandbox-unavailable shape=exec-denied execs=1 \
         denied=1 succeeded=0\n\
         # LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 category=SANDBOX_UNAVAILABLE \
         exit_code=0 model=none\n";

    const SUCCESS_BODY: &str =
        "exec\n/bin/bash -lc 'gh issue list' in /w\n succeeded in 9ms:\n[]\n\
         # LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 category=SUCCESS exit_code=0 \
         model=none\n";

    #[test]
    fn a_sandbox_unavailable_record_is_a_no_op_naming_the_shape() {
        let log = tick(ANCHOR, NOOP_BODY);
        let Verdict::NoOp(detail) = verdict_in(&log, "codex", ANCHOR) else {
            panic!("expected a no-op verdict");
        };
        assert!(detail.starts_with(REASON_PREFIX), "{detail}");
        assert!(detail.contains("(codex)"), "{detail}");
        assert!(detail.contains("shape=exec-denied execs=1 denied=1 succeeded=0"), "{detail}");
        assert_eq!(detail_for_hold(&detail), "shape=exec-denied execs=1 denied=1 succeeded=0");
    }

    #[test]
    fn a_success_record_means_a_tool_call_ran() {
        assert_eq!(verdict_in(&tick(ANCHOR, SUCCESS_BODY), "codex", ANCHOR), Verdict::Ran);
    }

    #[test]
    fn a_pre_10003_adapters_success_record_is_rescanned_from_the_tick_region() {
        // The installed adapter predates the verdict: the record says SUCCESS,
        // but this tick's own region shows every exec refused by bwrap.
        let body = "exec\n/bin/bash -lc 'cat SKILL.md' in /w\n exited 1 in 0ms:\n\
                    bwrap: No permissions to create a new namespace\n\
                    codex\nblocked: `bwrap: No permissions to create a new namespace`\n\
                    # LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 category=SUCCESS \
                    exit_code=0 model=none\n";
        let Verdict::NoOp(detail) = verdict_in(&tick(ANCHOR, body), "codex", ANCHOR) else {
            panic!("expected the in-process scan to catch the no-op");
        };
        assert!(detail.contains("shape=exec-denied execs=1 denied=1 succeeded=0"), "{detail}");
    }

    #[test]
    fn a_success_with_no_command_that_ran_is_no_opinion_and_keeps_the_hold() {
        // robb-studio 2026-10-03T05:50Z, loom/judge: every shell call failed
        // on a read-only sandbox registry lock, no exec was echoed, and the
        // record said SUCCESS. No no-op shape matches, but nothing ran either,
        // so this must not clear a hold a real no-op armed.
        let body = "codex\nThe Judge review is blocked: shell commands fail before execution \
                    because the sandbox's mount-registry lock is on a read-only filesystem.\n\
                    tokens used\n25,969\n\
                    # LOOM_TERMINAL_RESULT v=2 provider=codex account=robb category=SUCCESS \
                    exit_code=0 model=none\n";
        assert_eq!(verdict_in(&tick(ANCHOR, body), "codex", ANCHOR), Verdict::NoOpinion);
        // An ordinary failed command with nothing succeeding is no proof either.
        let body = "exec\n/bin/bash -lc 'cat nope' in /w\n exited 1 in 3ms:\n\
                    cat: nope: No such file or directory\n\
                    # LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 category=SUCCESS \
                    exit_code=0 model=none\n";
        assert_eq!(verdict_in(&tick(ANCHOR, body), "codex", ANCHOR), Verdict::NoOpinion);
    }

    #[test]
    fn prose_about_bwrap_without_the_record_is_no_opinion() {
        // A pre-#10003 adapter (or any session) whose record is not
        // SANDBOX_UNAVAILABLE is never failed on free text alone.
        let body = "codex\nblocked: `bwrap: No permissions to create a new namespace`\n\
                    # LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 category=RECOVERABLE \
                    exit_code=0 model=none\n";
        assert_eq!(verdict_in(&tick(ANCHOR, body), "codex", ANCHOR), Verdict::NoOpinion);
    }

    #[test]
    fn a_stale_no_op_from_an_earlier_tick_never_fails_this_one() {
        let earlier = tick("2026-10-02T20:00:00.000000Z", NOOP_BODY);
        let log = format!("{earlier}{}", tick(ANCHOR, SUCCESS_BODY));
        assert_eq!(verdict_in(&log, "codex", ANCHOR), Verdict::Ran);
        // ...and this tick having no record at all is no opinion, not the
        // earlier tick's no-op.
        let log = format!("{earlier}{}", tick(ANCHOR, "codex ran out of output\n"));
        assert_eq!(verdict_in(&log, "codex", ANCHOR), Verdict::NoOpinion);
    }

    #[test]
    fn only_codex_ticks_are_read() {
        let log = tick(ANCHOR, NOOP_BODY);
        assert_eq!(verdict_in(&log, "claude", ANCHOR), Verdict::NoOpinion);
        assert_eq!(verdict_in(&log, "opencode", ANCHOR), Verdict::NoOpinion);
        assert_eq!(verdict_in(&log, "codex", ""), Verdict::NoOpinion);
    }

    #[test]
    fn a_missing_detail_line_still_fails_the_tick() {
        let body = "# LOOM_TERMINAL_RESULT v=2 provider=codex account=agent-1 \
                    category=SANDBOX_UNAVAILABLE exit_code=0 model=none\n";
        let Verdict::NoOp(detail) = verdict_in(&tick(ANCHOR, body), "codex", ANCHOR) else {
            panic!("expected a no-op verdict");
        };
        assert!(detail.contains("no detail line"), "{detail}");
    }

    /// The end-to-end effect the issue asks for: a no-op tick arms the hold
    /// the preference walk reads, and a tick that ran a command clears it.
    #[test]
    #[serial_test::serial]
    fn apply_arms_the_hold_on_a_no_op_and_clears_it_on_a_real_run() {
        use crate::runtime_preference::sandbox_hold;
        /// Restores the env and drops the process-global hold even when an
        /// assertion panics, so no other serial test inherits it.
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                sandbox_hold::clear("codex");
                match self.0.take() {
                    Some(value) => std::env::set_var(sandbox_hold::HOLD_SECS_ENV, value),
                    None => std::env::remove_var(sandbox_hold::HOLD_SECS_ENV),
                }
            }
        }
        let _restore = Restore(std::env::var(sandbox_hold::HOLD_SECS_ENV).ok());
        std::env::set_var(sandbox_hold::HOLD_SECS_ENV, "600");
        sandbox_hold::clear("codex");

        let verdict = verdict_in(&tick(ANCHOR, NOOP_BODY), "codex", ANCHOR);
        let detail = apply(verdict, "codex", 1_000).expect("a no-op is a failure");
        assert!(detail.starts_with(REASON_PREFIX));
        let hold = sandbox_hold::active("codex", 1_001).expect("hold armed");
        assert_eq!(hold.until, 1_600);
        assert_eq!(hold.detail, "shape=exec-denied execs=1 denied=1 succeeded=0");

        assert_eq!(apply(Verdict::NoOpinion, "codex", 1_002), None);
        assert!(sandbox_hold::active("codex", 1_002).is_some(), "no opinion leaves it");

        assert_eq!(apply(Verdict::Ran, "codex", 1_003), None);
        assert_eq!(sandbox_hold::active("codex", 1_003), None, "a real run clears it");
    }
}
