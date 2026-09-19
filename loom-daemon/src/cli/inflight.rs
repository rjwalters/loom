//! `loom-daemon inflight` — the agent-facing surface onto the in-flight
//! verification registry ([`loom_daemon::inflight`], Issue #8268).
//!
//! No running daemon is required: state is a machine-wide directory, not
//! in-daemon memory. That is load-bearing rather than incidental — the
//! sessions this coordinates (an interactive coordinator, its subagents, a
//! headless sweep) routinely run on a host with no daemon at all, which is
//! exactly the topology #8268 was reported from.
//!
//! The sub-action enum lives here, not in `main.rs`, because that file is
//! frozen by the file-size ratchet (`.loom/docs/file-size-policy.md`) — the
//! same reason [`crate::cli::sweep_experiment`] is shaped this way.

use anyhow::Result;
use clap::Subcommand;

use loom_daemon::inflight::{
    self, fingerprint, normalize_tree, resolve_stale, store_dir, ClaimOutcome, Registration,
};

/// Exit code meaning "this command is already in flight — do not launch a
/// duplicate". Distinct from `1` (a real error) so a caller can branch on it.
const EXIT_BUSY: i32 = 10;

#[derive(Subcommand)]
pub(crate) enum InflightAction {
    /// Atomically claim a command fingerprint, then run the command yourself.
    ///
    /// Exit 0 = you own it, go run the command (release when done). Exit 10 =
    /// someone else is already running it against this tree; report theirs
    /// instead of starting a second copy. This is the verb to use when you
    /// intend to LAUNCH — it closes the check-then-launch race that a plain
    /// query cannot.
    Claim {
        /// The command you are about to run, as you would type it.
        #[arg(long, value_name = "CMD")]
        command: String,

        /// The working tree it verifies. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        tree: Option<String>,

        /// The branch/ref it verifies. Entries for different branches are
        /// deliberately distinct — a rebase genuinely warrants a re-run.
        #[arg(long, value_name = "REF", default_value = "")]
        branch: String,

        /// Who you are (role, session, sweep run id) — shown to whoever is
        /// told "already running".
        #[arg(long, value_name = "NAME", default_value = "")]
        agent: String,

        /// Liveness PID. Defaults to this process's parent, i.e. the shell
        /// that will actually run the command. Pass 0 to opt out and rely on
        /// age-based staleness alone.
        #[arg(long, value_name = "PID")]
        pid: Option<u32>,

        /// Emit JSON instead of human text.
        #[arg(long)]
        json: bool,
    },

    /// Report whether a command is in flight, without claiming it.
    ///
    /// ADVISORY ONLY. A clear result here is not permission to launch — two
    /// callers can both see clear and both start. Use `claim` for that.
    Check {
        #[arg(long, value_name = "CMD")]
        command: String,

        #[arg(long, value_name = "PATH")]
        tree: Option<String>,

        #[arg(long, value_name = "REF", default_value = "")]
        branch: String,

        #[arg(long)]
        json: bool,
    },

    /// Release a claim once your command has finished.
    Release {
        /// The fingerprint printed by `claim`.
        #[arg(value_name = "FINGERPRINT")]
        fingerprint: String,

        /// Your PID, checked against the recorded owner.
        #[arg(long, value_name = "PID")]
        pid: Option<u32>,

        /// Release even when the recorded owner is someone else.
        #[arg(long)]
        force: bool,
    },

    /// List every command currently in flight on this host.
    List {
        #[arg(long)]
        json: bool,
    },
}

impl InflightAction {
    /// Run the sub-action. Never returns `Err` for the ordinary "busy" answer
    /// — that is an exit code, not a failure.
    pub(crate) fn run(self) -> Result<()> {
        let Some(store) = store_dir() else {
            // No home directory and no override: there is no registry to
            // consult. Degrade open loudly rather than blocking the caller.
            eprintln!(
                "inflight: no store directory (set {} to enable) — proceeding unserialized",
                inflight::INFLIGHT_DIR_ENV
            );
            return Ok(());
        };
        let stale = resolve_stale();

        match self {
            Self::Claim {
                command,
                tree,
                branch,
                agent,
                pid,
                json,
            } => {
                let tree = normalize_tree(&tree.unwrap_or_else(default_tree));
                let reg = Registration {
                    fingerprint: fingerprint(&command, &tree, &branch),
                    command: command.clone(),
                    tree,
                    branch,
                    pid: pid.unwrap_or_else(default_pid),
                    agent,
                    started_at: chrono::Utc::now(),
                };
                match inflight::claim_in(&store, &reg, stale) {
                    ClaimOutcome::Claimed(r) => {
                        if json {
                            print_json(&serde_json::json!({"status": "claimed", "entry": r}));
                        } else {
                            println!("{}", r.fingerprint);
                            eprintln!(
                                "inflight: claimed — run your command, then: \
                                 loom-daemon inflight release {}",
                                r.fingerprint
                            );
                        }
                        Ok(())
                    }
                    ClaimOutcome::Busy(holder) => {
                        if json {
                            print_json(&serde_json::json!({"status": "busy", "entry": holder}));
                        } else {
                            eprintln!("inflight: ALREADY RUNNING — {}", holder.summary());
                            eprintln!(
                                "inflight: do not launch a duplicate; report the in-flight run \
                                 above, or wait for its result."
                            );
                        }
                        std::process::exit(EXIT_BUSY);
                    }
                    ClaimOutcome::DegradedOpen => {
                        if json {
                            print_json(&serde_json::json!({"status": "degraded-open"}));
                        } else {
                            eprintln!(
                                "inflight: registry unusable at {} — proceeding unserialized",
                                store.display()
                            );
                        }
                        Ok(())
                    }
                }
            }

            Self::Check {
                command,
                tree,
                branch,
                json,
            } => {
                let tree = normalize_tree(&tree.unwrap_or_else(default_tree));
                let fp = fingerprint(&command, &tree, &branch);
                match inflight::check_in(&store, &fp, stale) {
                    Some(holder) => {
                        if json {
                            print_json(&serde_json::json!({"status": "busy", "entry": holder}));
                        } else {
                            println!("in flight: {}", holder.summary());
                        }
                        std::process::exit(EXIT_BUSY);
                    }
                    None => {
                        if json {
                            print_json(&serde_json::json!({"status": "clear", "fingerprint": fp}));
                        } else {
                            println!("not in flight: {fp}");
                            eprintln!(
                                "inflight: advisory only — use `claim` if you intend to launch."
                            );
                        }
                        Ok(())
                    }
                }
            }

            Self::Release {
                fingerprint: fp,
                pid,
                force,
            } => {
                let removed =
                    inflight::release_in(&store, &fp, pid.or_else(|| Some(default_pid())), force);
                if removed {
                    println!("released {fp}");
                } else {
                    println!("nothing to release for {fp}");
                }
                Ok(())
            }

            Self::List { json } => {
                let entries = inflight::list_in(&store, stale);
                if json {
                    print_json(&serde_json::json!({"entries": entries}));
                } else if entries.is_empty() {
                    println!("no commands in flight");
                } else {
                    for e in &entries {
                        println!("{}", e.summary());
                    }
                }
                Ok(())
            }
        }
    }
}

/// The tree to fingerprint against when the caller did not name one.
fn default_tree() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string())
}

/// The liveness PID to record when the caller did not name one.
///
/// This process exits immediately, so its own PID would be dead before the
/// command it describes even starts. The parent — the shell or agent harness
/// that will run the command — is the right handle, mirroring
/// `sweep-run-registry.sh`'s "the recorded PID must name the long-lived
/// process, never this one-shot invocation" lesson (#4691).
fn default_pid() -> u32 {
    std::os::unix::process::parent_id()
}

fn print_json(value: &serde_json::Value) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("inflight: could not render JSON: {e}"),
    }
}
