//! The [`CmdOutcome`] bridge for migrated sites (#9985 slice 3).
//!
//! Most raw `gh` sites already speak `cmd_out`'s vocabulary — `run_command`
//! → [`CmdOutcome`] → `decode_json` — which keeps "it answered no" (a
//! non-zero exit) distinct from "we do not know" (spawn failure, collect
//! failure, deadline). [`GhInvocation::run`] gives a migrated site the same
//! classification, byte-for-byte `cmd_out::run_command`'s mapping, so a
//! migration changes only *who spawns*, never how the result is read.

use super::{GhCompletion, GhInvocation, OutputContract};
use crate::cmd_out::{CmdOutcome, Unavailable};
use crate::proc_exec::{Completion, ExecError};

impl GhInvocation {
    /// Execute a captured invocation and classify it as a [`CmdOutcome`]:
    /// exited (any status) ⇒ [`CmdOutcome::Ran`]; spawn failure, collect
    /// failure or the deadline ⇒ [`CmdOutcome::Unavailable`].
    ///
    /// A [`OutputContract::Passthrough`] (or
    /// [`OutputContract::CredentialHelper`]) invocation has no captured output to
    /// classify; it is **not run** (no side effects) and reported as
    /// [`Unavailable::Spawn`] — passthrough is never silently converted into
    /// captured output, nor the reverse.
    #[must_use]
    pub fn run(self) -> CmdOutcome {
        let OutputContract::Captured { timeout } = self.contract else {
            return CmdOutcome::Unavailable(Unavailable::Spawn(
                "a passthrough gh invocation has no captured outcome".to_string(),
            ));
        };
        classify(self.execute(), timeout)
    }

    /// [`GhInvocation::run`] for an async caller (#10089): the bounded,
    /// blocking execution moves to tokio's blocking pool so it never stalls
    /// a runtime worker. A join failure (the blocking task panicked) is
    /// [`Unavailable::Collect`] — no answer, never an invented one.
    pub async fn run_async(self) -> CmdOutcome {
        tokio::task::spawn_blocking(move || self.run())
            .await
            .unwrap_or_else(|e| {
                CmdOutcome::Unavailable(Unavailable::Collect(format!("gh task failed: {e}")))
            })
    }
}

/// `cmd_out::run_command`'s mapping, over the facade's result.
pub(super) fn classify(
    result: Result<GhCompletion, ExecError>,
    timeout: std::time::Duration,
) -> CmdOutcome {
    match result {
        Ok(GhCompletion::Captured(Completion::Exited(out))) => {
            // The managed launcher's own exit codes (#9987) are routing
            // outcomes, not forge answers: surface them as unavailable so no
            // caller retries them or reads them as an empty result.
            let detail = || String::from_utf8_lossy(&out.stderr).trim().to_string();
            match out.status.code() {
                Some(super::telemetry::EXIT_ROUTING_BLOCKED) => {
                    CmdOutcome::Unavailable(Unavailable::RoutingBlocked(detail()))
                }
                Some(super::telemetry::EXIT_ADAPTER_UNAVAILABLE) => {
                    CmdOutcome::Unavailable(Unavailable::AdapterUnavailable(detail()))
                }
                _ => CmdOutcome::Ran(out),
            }
        }
        Ok(GhCompletion::Captured(Completion::TimedOut { stdout, .. })) => {
            CmdOutcome::Unavailable(Unavailable::TimedOut {
                after: timeout,
                partial_stdout: stdout,
            })
        }
        // Unreachable for a captured contract; reported as "no answer" rather
        // than invented.
        Ok(GhCompletion::Passthrough(status)) => CmdOutcome::Unavailable(Unavailable::Collect(
            format!("passthrough completion ({status}) has no captured output"),
        )),
        Err(ExecError::Spawn(e)) => CmdOutcome::Unavailable(Unavailable::Spawn(e.to_string())),
        Err(ExecError::Collect(e)) => CmdOutcome::Unavailable(Unavailable::Collect(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::gh_invocation::{AccessIntent, GhBinSource, GhTarget, Operation, ParentContext};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn inv(timeout: Duration) -> GhInvocation {
        GhInvocation::new(
            Operation::new("api.rate_limit"),
            AccessIntent::Read,
            GhTarget::None,
            timeout,
        )
        .parent(ParentContext::Missing)
    }

    fn stub(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("gh-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn run_with(inv: GhInvocation, program: &str) -> CmdOutcome {
        let OutputContract::Captured { timeout } = inv.contract else {
            panic!("captured contract expected");
        };
        classify(inv.execute_with(program, GhBinSource::EnvOverride), timeout)
    }

    #[test]
    fn a_non_zero_exit_is_an_answer_not_an_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run_with(inv(TIMEOUT), &stub(tmp.path(), "echo nope >&2; exit 4"));
        let CmdOutcome::Ran(o) = out else {
            panic!("expected Ran, got {out:?}");
        };
        assert_eq!(o.status.code(), Some(4));
        assert_eq!(String::from_utf8_lossy(&o.stderr).trim(), "nope");
    }

    /// [`run_with`] on a fresh stub, waiting out `ETXTBSY`: a concurrent test
    /// thread that forked while the stub's write fd was open holds it until
    /// its exec, and Linux refuses to exec the script meanwhile (#10446 CI).
    fn run_stub(dir: &std::path::Path, body: &str) -> CmdOutcome {
        let program = stub(dir, body);
        for _ in 0..100 {
            match run_with(inv(TIMEOUT), &program) {
                CmdOutcome::Unavailable(Unavailable::Spawn(e)) if e.contains("os error 26") => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                out => return out,
            }
        }
        run_with(inv(TIMEOUT), &program)
    }

    #[test]
    fn launcher_exit_78_is_routing_blocked_and_69_is_adapter_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run_stub(tmp.path(), "echo denied >&2; exit 78");
        assert!(
            matches!(&out, CmdOutcome::Unavailable(Unavailable::RoutingBlocked(m)) if m == "denied"),
            "{out:?}"
        );
        assert!(!out.succeeded());
        assert_eq!(out.stdout_lossy(), "");
        let out = run_stub(tmp.path(), "exit 69");
        assert!(
            matches!(out, CmdOutcome::Unavailable(Unavailable::AdapterUnavailable(_))),
            "{out:?}"
        );
        // Every other non-zero status is still an ordinary answer.
        let out = run_stub(tmp.path(), "exit 70");
        assert!(matches!(out, CmdOutcome::Ran(_)), "{out:?}");
    }

    #[test]
    fn a_zero_exit_carries_stdout_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let out = run_with(inv(TIMEOUT), &stub(tmp.path(), "printf '{\"a\":1}'"));
        let CmdOutcome::Ran(o) = out else {
            panic!("expected Ran, got {out:?}");
        };
        assert!(o.status.success());
        assert_eq!(o.stdout, b"{\"a\":1}");
    }

    #[test]
    fn spawn_failure_and_timeout_are_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-gh");
        assert!(matches!(
            run_with(inv(TIMEOUT), &missing.to_string_lossy()),
            CmdOutcome::Unavailable(Unavailable::Spawn(_))
        ));
        let slow = Duration::from_millis(300);
        assert!(matches!(
            run_with(inv(slow), &stub(tmp.path(), "sleep 5")),
            CmdOutcome::Unavailable(Unavailable::TimedOut { after, .. }) if after == slow
        ));
    }

    #[test]
    fn run_async_classifies_exactly_like_run() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = stub(tmp.path(), "printf ok; exit 3");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(inv(TIMEOUT).program(&gh).run_async());
        let CmdOutcome::Ran(o) = out else {
            panic!("expected Ran, got {out:?}");
        };
        assert_eq!((o.status.code(), &o.stdout[..]), (Some(3), &b"ok"[..]));
        let missing = tmp.path().join("no-such-gh");
        assert!(matches!(
            rt.block_on(inv(TIMEOUT).program(&missing).run_async()),
            CmdOutcome::Unavailable(Unavailable::Spawn(_))
        ));
    }

    #[test]
    fn a_passthrough_invocation_is_never_run_as_captured() {
        assert!(matches!(
            inv(TIMEOUT).passthrough().run(),
            CmdOutcome::Unavailable(Unavailable::Spawn(_))
        ));
    }
}
