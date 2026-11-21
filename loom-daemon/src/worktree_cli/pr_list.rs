//! Read transport for the shared worktree branch probes.

use std::path::Path;
use std::process::{Command, Output};

use crate::forge_cmd::{detect_forge, ForgeType, FORGE_CMD_TIMEOUT};
use crate::fs_util::is_executable;
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::proc_exec::Completion;

#[derive(Clone, Copy)]
pub(crate) enum PrList {
    Legacy,
    Tools,
}

/// A transport outcome. Invalid tools routing returns a command error instead.
pub(crate) enum Read {
    Answered(String),
    Unanswered(String),
}

impl PrList {
    pub(crate) fn fetch(
        self,
        mode_repo: &Path,
        git_repo: &Path,
        args: &[&str],
    ) -> Result<Read, String> {
        let output = match self {
            Self::Legacy => legacy_output(git_repo, args),
            Self::Tools => {
                // Mode is required when the probe reaches the forge. Linked
                // worktrees still use the primary checkout's effective mode.
                crate::tools_mode::configure_forge_reads(mode_repo)?;
                if detect_forge(Some(git_repo)) == ForgeType::Gitea {
                    return Ok(Read::Unanswered("gitea is not handled natively".into()));
                }
                tools_output(git_repo, args)
            }
        };
        Ok(match output {
            Ok(output) if output.status.success() => {
                Read::Answered(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Ok(output) => Read::Unanswered(String::from_utf8_lossy(&output.stderr).into_owned()),
            Err(error) => Read::Unanswered(error),
        })
    }
}

fn legacy_output(repo: &Path, args: &[&str]) -> Result<Output, String> {
    // Keep the existing daemon/gh selection for callers of the legacy CLI.
    let (program, leading): (&str, &[&str]) = if on_path("loom-daemon") {
        ("loom-daemon", &["forge"])
    } else if on_path("gh") {
        ("gh", &[])
    } else {
        return Err("no forge client on PATH".into());
    };
    Command::new(program)
        .args(leading)
        .args(["pr", "list"])
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|err| err.to_string())
}

fn tools_output(repo: &Path, args: &[&str]) -> Result<Output, String> {
    let completed = GhInvocation::new(
        Operation::new("worktree.pr_list"),
        AccessIntent::Read,
        GhTarget::None,
        FORGE_CMD_TIMEOUT,
    )
    .args(["pr", "list"])
    .args(args)
    .current_dir(repo)
    .execute()
    .map_err(|err| err.to_string())?;
    match completed {
        GhCompletion::Captured(Completion::Exited(output)) => Ok(output),
        _ => Err("forge PR list was unavailable".into()),
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}
