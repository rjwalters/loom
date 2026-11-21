//! Tools dispatch for the shared issue and role-keyed PR lease command tree.
//! Issue leases preserve their per-verb mode failure contracts (#14488).
//! PR leases select their own workspace and route (#14489).

use anyhow::Result;

use super::lease_ensure::{LeaseCommand, SessionEnv};
use super::lease_renewer::RenewerAction;

/// `loom-tools lease <verb>`: the [`LeaseCommand`] tree, unchanged. The field is
/// private so the crate-private command type does not leak into the public API.
#[derive(clap::Args)]
pub struct LeaseCli {
    #[command(subcommand)]
    command: LeaseCommand,
}

impl LeaseCli {
    /// Whether the tools entry point must resolve mode in the current directory
    /// for an issue-lease invocation. PR leases resolve their selected
    /// workspace's mode in `PrLeaseAction::run_tools`.
    ///
    /// `ensure` reaches it unless its answer is settled from the environment
    /// alone (no agent session, or the daemon already dispatched it): those two
    /// print their message and start nothing, with `mode` unset. `co-occupancy`
    /// reads the issue's lease comments, `release` reads them and posts the
    /// yield. Every `renewer` action touches only the renewer ownership files
    /// and process table or local diagnostics. `check` receives the issue state from its caller.
    #[must_use]
    pub fn needs_issue_mode_gate(&self) -> bool {
        match &self.command {
            LeaseCommand::Ensure(args) => args.reaches_forge(&SessionEnv::from_process()),
            LeaseCommand::CoOccupancy(_) | LeaseCommand::Release(_) => true,
            LeaseCommand::Renewer(action) => match action {
                RenewerAction::Claim { .. }
                | RenewerAction::Check { .. }
                | RenewerAction::Release { .. }
                | RenewerAction::Stop { .. }
                | RenewerAction::SanitizeExec { .. }
                | RenewerAction::TerminalReceipt(_) => false,
            },
            LeaseCommand::Pr(_) => false,
        }
    }

    /// Whether a failure to resolve `mode` must leave the verb's own exit
    /// contract intact instead of exiting 1.
    ///
    /// `ensure` "always exits 0" (a lease is evidence, never a precondition) and
    /// `co-occupancy` fails open with 0 on any read error, because its exit 1 is
    /// the one signal `worktree.sh` treats as "two live sweeps share this
    /// worktree" and refuses on. A mode error must not alias either. `release`
    /// already exits 1 when it cannot read the lease comments, so it reports
    /// the mode error the usual way.
    #[must_use]
    pub fn fails_open_on_mode_error(&self) -> bool {
        match &self.command {
            LeaseCommand::Ensure(_) | LeaseCommand::CoOccupancy(_) => true,
            LeaseCommand::Release(_) | LeaseCommand::Renewer(_) | LeaseCommand::Pr(_) => false,
        }
    }

    /// The verb as typed, for diagnostics.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match &self.command {
            LeaseCommand::Ensure(_) => "ensure",
            LeaseCommand::CoOccupancy(_) => "co-occupancy",
            LeaseCommand::Release(_) => "release",
            LeaseCommand::Renewer(_) => "renewer",
            LeaseCommand::Pr(_) => "pr",
        }
    }
}

/// Dispatch lease commands through the handlers shared with the daemon.
///
/// # Errors
/// A handler can return an error, including an invalid PR lease workspace mode.
pub fn handle_lease_command(cli: LeaseCli) -> Result<()> {
    match cli.command {
        LeaseCommand::Pr(action) => action.run_tools(),
        command @ (LeaseCommand::Ensure(_)
        | LeaseCommand::CoOccupancy(_)
        | LeaseCommand::Release(_)
        | LeaseCommand::Renewer(_)) => command.run(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Root {
        #[command(flatten)]
        lease: LeaseCli,
    }

    /// Run `f` on an 8 MiB thread, like a binary's main thread: the lease tree
    /// is nested under `PrLeaseAction`, which does not fit libtest's 2 MiB
    /// worker in a debug build.
    fn big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    fn parse(tail: &[&str]) -> LeaseCli {
        let argv: Vec<String> = std::iter::once("lease".to_string())
            .chain(tail.iter().map(|s| (*s).to_string()))
            .collect();
        Root::try_parse_from(&argv)
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
            .lease
    }

    #[test]
    fn issue_mode_gate_is_classified_by_effect() {
        big_stack(|| {
            // (argv tail, uses the issue-mode gate, fails open on a mode error)
            let table: [(&[&str], bool, bool); 9] = [
                (&["co-occupancy", "7"], true, true),
                (&["release", "7"], true, false),
                (&["renewer", "stop", "--issue", "7"], false, false),
                (&["renewer", "release", "7"], false, false),
                (&["renewer", "sanitize-exec", "--check"], false, false),
                (&["renewer", "claim", "7", "--pid", "1", "--token", "t"], false, false),
                (&["renewer", "check", "7", "--token", "t"], false, false),
                // A forced ensure always reaches the publish script.
                (&["ensure", "7", "--watch-pid", "1", "--force"], true, true),
                // The PR handler applies its own workspace-specific gate.
                (&["pr", "check", "7", "--role", "judge"], false, false),
            ];
            for (tail, issue_gate, open) in table {
                let cli = parse(tail);
                assert_eq!(cli.needs_issue_mode_gate(), issue_gate, "{tail:?}");
                assert_eq!(cli.fails_open_on_mode_error(), open, "{tail:?}");
            }
        });
    }

    #[test]
    fn ensure_settled_by_the_environment_alone_never_needs_mode() {
        use super::super::lease_ensure::LeaseEnsureArgs;
        big_stack(|| {
            let cli = parse(&["ensure", "7", "--watch-pid", "1"]);
            let LeaseCommand::Ensure(args) = cli.command else {
                panic!("not ensure")
            };
            let args: LeaseEnsureArgs = args;
            let none = SessionEnv::default();
            assert!(!args.reaches_forge(&none), "no agent session settles it offline");
            let dispatched = SessionEnv {
                dispatched_issue: Some("7".into()),
                session_present: true,
                ..SessionEnv::default()
            };
            assert!(!args.reaches_forge(&dispatched), "already dispatched settles it offline");
            let other = SessionEnv {
                dispatched_issue: Some("8".into()),
                session_present: true,
                ..SessionEnv::default()
            };
            assert!(args.reaches_forge(&other), "a different issue's marker does not");
            let session = SessionEnv {
                session_present: true,
                ..SessionEnv::default()
            };
            assert!(args.reaches_forge(&session), "a live session publishes");
        });
    }
}
