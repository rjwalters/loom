//! `loom-tools lease` — the issue lease verbs behind both binaries (#14488,
//! epic #12708 Phase 3, design `docs/design/loom-tools-cli-boundary.md` §15).
//!
//! [`LeaseCli`] wraps the one [`LeaseCommand`] clap tree `loom-daemon lease`
//! parses, and [`handle_lease_command`] runs the one in-process handler, so
//! `loom-tools` and `loom-daemon` cannot drift. Nothing is copied. The served
//! verbs are `ensure`, `co-occupancy`, `release` and every `renewer` action
//! (`claim`, `check`, `release`, `stop`, `sanitize-exec`). `lease pr` — the
//! role-keyed PR leases — is not an issue lease and stays in `loom-daemon`
//! (#14398): it is refused with exit 64 before any handler runs.
//!
//! [`LeaseCli::reaches_forge`] decides whether `loom-tools` must resolve `mode`
//! first. It is classified by effect, from the parsed invocation, and every
//! `match` is exhaustive with no wildcard arm: a new verb does not compile
//! until its author says which side it is on.

use anyhow::Result;

use super::forge_read::EX_WRITE_VERB_NOT_MIGRATED;
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
    /// Whether `loom-tools` serves this verb. `false` for `lease pr`.
    #[must_use]
    pub fn is_served(&self) -> bool {
        match &self.command {
            LeaseCommand::Ensure(_)
            | LeaseCommand::CoOccupancy(_)
            | LeaseCommand::Release(_)
            | LeaseCommand::Renewer(_) => true,
            LeaseCommand::Pr(_) => false,
        }
    }

    /// Whether this invocation can reach the forge, and so must pass the mode
    /// gate (design §10.3) before it runs.
    ///
    /// `ensure` reaches it unless its answer is settled from the environment
    /// alone (no agent session, or the daemon already dispatched it): those two
    /// print their message and start nothing, with `mode` unset. `co-occupancy`
    /// reads the issue's lease comments, `release` reads them and posts the
    /// yield. Every `renewer` action touches only the renewer ownership files
    /// and process table; `check` is handed the issue state by its caller.
    #[must_use]
    pub fn reaches_forge(&self) -> bool {
        match &self.command {
            LeaseCommand::Ensure(args) => args.reaches_forge(&SessionEnv::from_process()),
            LeaseCommand::CoOccupancy(_) | LeaseCommand::Release(_) => true,
            LeaseCommand::Renewer(action) => match action {
                RenewerAction::Claim { .. }
                | RenewerAction::Check { .. }
                | RenewerAction::Release { .. }
                | RenewerAction::Stop { .. }
                | RenewerAction::SanitizeExec { .. } => false,
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

/// Run the lease verb through the one shared handler. Refuses (exit
/// [`EX_WRITE_VERB_NOT_MIGRATED`]) a verb `loom-tools` does not serve.
///
/// # Errors
/// Whatever the handler returns; `release` and `co-occupancy` exit the process
/// themselves with their documented codes.
pub fn handle_lease_command(cli: LeaseCli) -> Result<()> {
    if !cli.is_served() {
        eprintln!(
            "loom-tools lease: `{}` is the role-keyed PR lease family and is not served by \
             loom-tools (#14398); use `loom-daemon lease {}`.",
            cli.verb(),
            cli.verb()
        );
        std::process::exit(EX_WRITE_VERB_NOT_MIGRATED);
    }
    cli.command.run()
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
    fn served_verbs_are_classified_by_effect() {
        big_stack(|| {
            // (argv tail, served, reaches the forge, fails open on a mode error)
            let table: [(&[&str], bool, bool, bool); 9] = [
                (&["co-occupancy", "7"], true, true, true),
                (&["release", "7"], true, true, false),
                (&["renewer", "stop", "--issue", "7"], true, false, false),
                (&["renewer", "release", "7"], true, false, false),
                (&["renewer", "sanitize-exec", "--check"], true, false, false),
                (&["renewer", "claim", "7", "--pid", "1", "--token", "t"], true, false, false),
                (&["renewer", "check", "7", "--token", "t"], true, false, false),
                // A forced ensure always reaches the publish script.
                (&["ensure", "7", "--watch-pid", "1", "--force"], true, true, true),
                // The role-keyed PR leases are not an issue lease.
                (&["pr", "check", "7", "--role", "judge"], false, false, false),
            ];
            for (tail, served, forge, open) in table {
                let cli = parse(tail);
                assert_eq!(cli.is_served(), served, "{tail:?}");
                assert_eq!(cli.reaches_forge(), forge, "{tail:?}");
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
