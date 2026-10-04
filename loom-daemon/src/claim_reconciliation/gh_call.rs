//! The one way the claim-reconciliation pass family spawns `gh` (#10089).
//!
//! Every site here used to hand-build a `Command::new(gh_bin)`, so none of
//! its calls reached [`crate::forge_call_stats`] and the breaker booked them
//! as "external". Routing them through [`GhInvocation`] counts each under a
//! stable `claim.*` / `verdict.*` / `sequence.*` operation name, and gives
//! every call a deadline (they were unbounded `.output()`s).
//!
//! The facade supplies what each site applied by hand: the root's
//! cross-owner `GH_CONFIG_DIR` (from the working directory) and `LOOM_REPO`
//! as `GH_REPO` (the #8263 contract for `gh api`). `gh issue|pr` sites keep
//! their `--repo` flag too ([`loom_repo_flag`]).

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Per-call deadline. Generous: a paginated comments walk on a long PR is
/// several requests.
pub(crate) const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// A facade invocation running `gh_bin` (a test stub, or bare `gh` in
/// production — resolved through the facade's ladder) in `root`.
pub(crate) fn inv(
    op: &'static str,
    intent: AccessIntent,
    gh_bin: &Path,
    root: &Path,
) -> GhInvocation {
    GhInvocation::new(Operation::new(op), intent, GhTarget::None, GH_TIMEOUT)
        .program(gh_bin)
        .current_dir(root)
}

/// [`inv`] with its own deadline instead of [`GH_TIMEOUT`].
pub(crate) fn read_within(
    op: &'static str,
    gh_bin: &Path,
    root: &Path,
    timeout: Duration,
) -> GhInvocation {
    GhInvocation::new(Operation::new(op), AccessIntent::Read, GhTarget::None, timeout)
        .program(gh_bin)
        .current_dir(root)
}

/// A read ([`AccessIntent::Read`]) — the common case.
pub(crate) fn read(op: &'static str, gh_bin: &Path, root: &Path) -> GhInvocation {
    inv(op, AccessIntent::Read, gh_bin, root)
}

/// A write ([`AccessIntent::Write`]).
pub(crate) fn write(op: &'static str, gh_bin: &Path, root: &Path) -> GhInvocation {
    inv(op, AccessIntent::Write, gh_bin, root)
}

/// `["--repo", $LOOM_REPO]` when the override is set, else nothing — for
/// `gh issue|pr` only (`gh api` has no `--repo`, #8263).
pub(crate) fn loom_repo_flag() -> Vec<String> {
    std::env::var("LOOM_REPO")
        .map(|repo| vec!["--repo".to_string(), repo])
        .unwrap_or_default()
}

/// Run `inv`: the process's output when `gh` ran (any exit status), an
/// error when it could not be started, collected, or outlived its deadline.
pub(crate) fn output(inv: GhInvocation) -> Result<Output> {
    let op = inv.operation().as_str();
    match inv.run() {
        CmdOutcome::Ran(out) => Ok(out),
        CmdOutcome::Unavailable(u) => Err(anyhow!("failed to invoke gh ({op}): {u}")),
    }
}

/// [`output`], `None` on any failure or non-zero exit — the fail-open shape
/// of the best-effort probes.
pub(crate) fn ok_stdout(inv: GhInvocation) -> Option<Vec<u8>> {
    output(inv)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}

/// The trimmed stderr of a failed run, for error messages.
pub(crate) fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}
