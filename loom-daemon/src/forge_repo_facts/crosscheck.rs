//! The one-time cross-check of an ambiguous root against gh's own resolver.
//!
//! The local resolver models gh's order but not everything gh does (ssh host
//! aliases, `insteadOf` rewrites, GHE host matching). On an ambiguous root the
//! first resolution per config fingerprint asks gh itself, with the SAME
//! command the replaced site ran, so each [`GhRepoEnv`] is checked against
//! its own semantics:
//!
//! - `Honour`: `gh api --include repos/{owner}/{repo}` — the facade maps
//!   `LOOM_REPO` to `GH_REPO` exactly as for every placeholder call;
//! - `Ignore`: `gh repo view --json nameWithOwner` with `GH_REPO` removed from
//!   the child, as `gh repo view` itself ignores it.

use std::path::Path;
use std::time::Duration;

use crate::cmd_out::CmdOutcome;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

use super::GhRepoEnv;

const CROSSCHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// gh's own `owner/name` for `root` under `env`; `None` when gh could not
/// answer.
pub(super) fn gh_answer(gh: &Path, root: &Path, env: GhRepoEnv) -> Option<String> {
    let inv = GhInvocation::new(
        Operation::new("repo_facts.crosscheck"),
        AccessIntent::Read,
        GhTarget::None,
        CROSSCHECK_TIMEOUT,
    )
    .forge_op(crate::forge_call_stats::ops::REPO_VIEW)
    .program(gh)
    .current_dir(root);
    match env {
        GhRepoEnv::Honour => {
            let CmdOutcome::Ran(out) = inv.args(["api", "--include", "repos/{owner}/{repo}"]).run()
            else {
                return None;
            };
            let response =
                crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout))?;
            if response.status != 200 {
                return None;
            }
            let v: serde_json::Value = serde_json::from_str(response.body.trim()).ok()?;
            let full = v.get("full_name")?.as_str()?.trim();
            full.contains('/').then(|| full.to_string())
        }
        GhRepoEnv::Ignore => {
            let out = inv
                .args([
                    "repo",
                    "view",
                    "--json",
                    "nameWithOwner",
                    "--jq",
                    ".nameWithOwner",
                ])
                .strip_env("GH_REPO")
                .run();
            let out = out.ok_output()?;
            let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
            full.contains('/').then_some(full)
        }
    }
}
