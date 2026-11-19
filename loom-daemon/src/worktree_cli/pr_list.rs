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

impl PrList {
    pub(crate) fn fetch(self, repo: &Path, args: &[&str]) -> Result<String, String> {
        let output = match self {
            Self::Legacy => legacy_output(repo, args)?,
            Self::Tools => tools_output(repo, args)?,
        };
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
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
    if detect_forge(Some(repo)) == ForgeType::Gitea {
        return Err("gitea is not handled natively".into());
    }
    // Resolve mode only on an actual forge read. Guards that finish using
    // local Git and filesystem evidence can run without a configured mode.
    if let Err(message) = crate::tools_mode::configure_forge_reads(repo) {
        eprintln!("{message}");
        return Err(message);
    }
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
