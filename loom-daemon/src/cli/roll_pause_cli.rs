//! `loom-daemon roll-pause …` and `loom-daemon agent-resume …` (issue #10830):
//! the agent-side half of pause-and-roll. The logic lives in
//! [`loom_daemon::roll_pause`]; the hooks and spawn scripts only call it.
//!
//! Every subcommand a hook or spawn script calls is safe against an older
//! binary that lacks it: the callers read stdout and treat clap's
//! "unrecognized subcommand" (exit 2, empty stdout) as "nothing to do".

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use loom_daemon::roll_pause::{self, resume, HookEnv, HookOutcome, PauseRequest};

/// Pause-hook and pause-state subcommands.
#[derive(clap::Subcommand)]
pub(crate) enum RollPauseCommand {
    /// The pre/post-tool-use hook (stdin: the harness payload). Inert unless
    /// LOOM_DAEMON_ITEM_ID is set. Always exits 0; a deny is JSON on stdout.
    Hook {
        /// The harness process (the hook script's parent), for the record.
        #[arg(long)]
        harness_pid: Option<u32>,
    },
    /// Exit 0 when a pause request is active for this agent's
    /// LOOM_DAEMON_ITEM_ID, 1 otherwise (the Stop guard's question).
    Active,
    /// Raise a pause request for an item (daemon side; also the live test).
    Request(ItemArgs),
    /// Withdraw an item's pause request, releasing parked calls.
    Withdraw(ItemArgs),
    /// Print an item's safe-point record; exit 1 when there is none yet.
    SafePoint(ItemArgs),
    /// Print the prompt a resumed session receives.
    ResumePrompt {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        /// A safe-point record to name the parked call from.
        #[arg(long)]
        safe_point: Option<PathBuf>,
    },
}

/// An item's state directory.
#[derive(clap::Args)]
pub(crate) struct ItemArgs {
    /// The item id (the sweep id for a sweep).
    #[arg(long)]
    item: String,
    /// Pause root (default: $LOOM_ROLL_PAUSE_DIR, else <cwd>/.loom/state/roll-pause).
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: Option<String>,
}

impl ItemArgs {
    fn item_dir(&self) -> Result<PathBuf> {
        anyhow::ensure!(roll_pause::valid_item_id(&self.item), "invalid item id {:?}", self.item);
        let root = match (
            &self.dir,
            std::env::var(roll_pause::DIR_ENV)
                .ok()
                .filter(|d| !d.is_empty()),
        ) {
            (Some(d), _) => d.clone(),
            (None, Some(d)) => PathBuf::from(d),
            (None, None) => roll_pause::default_pause_root(&std::env::current_dir()?),
        };
        Ok(roll_pause::item_dir(&root, &self.item))
    }
}

impl RollPauseCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            RollPauseCommand::Hook { harness_pid } => {
                let mut payload = String::new();
                let _ = std::io::stdin().read_to_string(&mut payload);
                let outcome = roll_pause::run_hook(&HookEnv::from_env(harness_pid), &payload);
                if let Some(json) = outcome.to_json() {
                    println!("{json}");
                }
                debug_assert!(matches!(outcome, HookOutcome::Allow | HookOutcome::Deny(_)));
                Ok(())
            }
            RollPauseCommand::Active => {
                let env = HookEnv::from_env(None);
                let active = env.item.as_deref().is_some_and(|i| {
                    roll_pause::is_requested(&roll_pause::item_dir(&env.pause_root, i))
                });
                std::process::exit(i32::from(!active));
            }
            RollPauseCommand::Request(args) => {
                let request = PauseRequest {
                    requested_at: chrono::Utc::now().to_rfc3339(),
                    from_version: args.from.clone(),
                    to_version: args.to.clone(),
                    manifest_id: None,
                };
                roll_pause::request_pause(&args.item_dir()?, &request)?;
                Ok(())
            }
            RollPauseCommand::Withdraw(args) => Ok(roll_pause::withdraw(&args.item_dir()?)?),
            RollPauseCommand::SafePoint(args) => {
                match roll_pause::read_safe_point(&args.item_dir()?) {
                    Some(sp) => {
                        println!("{}", serde_json::to_string(&sp)?);
                        Ok(())
                    }
                    None => std::process::exit(1),
                }
            }
            RollPauseCommand::ResumePrompt {
                from,
                to,
                safe_point,
            } => {
                let sp = safe_point
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .and_then(|raw| serde_json::from_str::<roll_pause::SafePoint>(&raw).ok());
                let input = resume::ResumePromptInput {
                    from_version: from,
                    to_version: to,
                    parked_tool: sp.as_ref().map(|s| s.parked_tool.clone()),
                    parked_summary: sp.map(|s| s.parked_summary),
                };
                println!("{}", resume::resume_prompt(&input));
                Ok(())
            }
        }
    }
}

/// Spawn-script session-handle subcommands.
#[derive(clap::Subcommand)]
pub(crate) enum AgentResumeCommand {
    /// The arguments spawn-claude.sh appends, NUL-separated: `--resume <id>
    /// <prompt>` (LOOM_RESUME_SESSION_ID + LOOM_RESUME_PROMPT), `--session-id
    /// <id>` (LOOM_CLAUDE_SESSION_ID), or nothing; then, for a daemon item
    /// (LOOM_DAEMON_ITEM_ID), `--settings <json>` wiring the roll-pause hook
    /// unless the launch dir's settings already do (#11049). Exit 78 on a
    /// bad session value.
    ClaudeArgs,
    /// Check a Codex resume launch (session id, prompt, LOOM_CODEX_HOME
    /// pinned) and print its prompt. Exit 78 when it is not resumable.
    CodexPrompt,
    /// Watch a Codex stderr capture and write the resume handle as soon as
    /// `session id:` appears. Ends with the watched pid.
    CaptureCodex {
        #[arg(long)]
        stderr_file: PathBuf,
        #[arg(long)]
        handle_file: PathBuf,
        #[arg(long)]
        watch_pid: Option<u32>,
        #[arg(long)]
        codex_home: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        container: Option<String>,
        /// The Codex sandbox mode the session was launched with (#10831), so a
        /// resume can relaunch it under the same one. `spawn-codex.sh` passes
        /// it as `LOOM_CODEX_SANDBOX_MODE` rather than as this flag, so a
        /// script newer than the binary it runs does not break the capture.
        #[arg(long)]
        sandbox: Option<String>,
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
    },
}

fn env_var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

impl AgentResumeCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            AgentResumeCommand::ClaudeArgs => match resume::claude_args_from_env() {
                Ok(args) => {
                    let mut out = std::io::stdout().lock();
                    for a in args {
                        out.write_all(a.as_bytes())?;
                        out.write_all(b"\0")?;
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("agent-resume: {e} (#10830)");
                    std::process::exit(78);
                }
            },
            AgentResumeCommand::CodexPrompt => {
                let id = env_var(resume::RESUME_SESSION_ENV).unwrap_or_default();
                match resume::codex_resume_prompt(
                    &id,
                    env_var(resume::RESUME_PROMPT_ENV).as_deref(),
                    env_var("LOOM_CODEX_HOME").as_deref(),
                ) {
                    Ok(prompt) => {
                        print!("{prompt}");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("agent-resume: {e} (#10830)");
                        std::process::exit(78);
                    }
                }
            }
            AgentResumeCommand::CaptureCodex {
                stderr_file,
                handle_file,
                watch_pid,
                codex_home,
                account,
                container,
                sandbox,
                timeout_secs,
            } => {
                let spec = resume::CaptureSpec {
                    stderr_file,
                    handle_file,
                    watch_pid,
                    poll: Duration::from_millis(250),
                    timeout: Duration::from_secs(timeout_secs),
                    template: resume::CapturedHandle {
                        session_store: codex_home.filter(|s| !s.is_empty()),
                        account: account.filter(|s| !s.is_empty()),
                        container: container.filter(|s| !s.is_empty()),
                        sandbox: sandbox
                            .or_else(|| env_var("LOOM_CODEX_SANDBOX_MODE"))
                            .filter(|s| !s.is_empty() && s != "unknown"),
                        cwd: std::env::current_dir()
                            .ok()
                            .map(|d| d.display().to_string()),
                        ..resume::CapturedHandle::default()
                    },
                };
                // Before the first poll: this watcher must not hold a private
                // account's lease past the end of the run (see the callee).
                resume::release_inherited_lease(env_var(resume::PRIVATE_LEASE_FD_ENV).as_deref());
                let _ = resume::capture_codex(&spec);
                Ok(())
            }
        }
    }
}
