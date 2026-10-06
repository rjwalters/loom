//! [`super::Invariant::GhFrontWired`] (#10516): interactive sessions reach the
//! agent `gh` front.
//!
//! Two halves, both mechanically checkable:
//!
//! 1. **Wired** — the `SessionStart` entry for `gh-front-env.sh` is in the
//!    user-scope `~/.claude/settings.json` (the `provision-hooks.sh` form) or
//!    in the repo's `.claude/settings.json` (the per-repo-copy fallback). A
//!    host whose user-scope settings carry no Loom hook at all was never
//!    provisioned, which is not this check's business: `Skipped`.
//! 2. **Effective** — the prefix `gh-shim session-env` would write resolves
//!    `gh` to a `loom-daemon` front, or to the managed launcher under a
//!    policy (#9987), exactly as a worker's `PATH` does.
//!
//! Not auto-repairable: re-provisioning rewrites the operator's user-scope
//! settings, which stays a deliberate act (`loom update`).

use std::ffi::OsString;
use std::path::Path;

use super::InvariantStatus;
use crate::agent_gh::session_env;

/// The user-scope (machine-checkout) wiring marker, as `provision-hooks.sh`
/// writes it.
const USER_MARKER: &str = "defaults/hooks/gh-front-env.sh";
/// The per-repo fallback's marker.
const PROJECT_MARKER: &str = ".loom/hooks/gh-front-env.sh";
/// Any Loom user-scope hook: present ⇒ the host was provisioned.
const ANY_LOOM_USER_HOOK: &str = "/defaults/hooks/";

/// Live check: reads the real settings files and computes the real prefix.
/// Skipped in a unit-test build so a developer's own `~/.claude` can never
/// change a test's outcome (the pure half is [`check_with`]).
pub(super) fn check(repo_root: &Path) -> InvariantStatus {
    if cfg!(test) {
        return InvariantStatus::Skipped("unit-test build".to_string());
    }
    if std::env::var(crate::agent_gh::OPT_OUT_ENV).is_ok_and(|v| v == "0") {
        return InvariantStatus::Skipped("LOOM_GH_SHIM=0 (opted out)".to_string());
    }
    if crate::forge_cmd::detect_forge(Some(repo_root)) == crate::forge_cmd::ForgeType::Gitea {
        return InvariantStatus::Skipped("Gitea forge: the gh front does not apply".to_string());
    }
    let read = |p: &Path| std::fs::read_to_string(p).ok();
    let user = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|h| read(&Path::new(&h).join(".claude/settings.json")));
    let project = read(&repo_root.join(".claude/settings.json"));
    let egress = crate::forge_egress::worker_env::WorkerEgress::admit_process()
        .ok()
        .and_then(|a| a.egress);
    let prefix = crate::agent_gh::session_path(None, egress.as_ref());
    check_with(
        user.as_deref(),
        project.as_deref(),
        prefix,
        egress.as_ref().map(|e| e.launcher.as_path()),
    )
}

/// The pure decision: `user` / `project` are the settings files' contents,
/// `prefix` what `gh-shim session-env` would prepend, `launcher` the policy's
/// managed launcher.
#[must_use]
pub fn check_with(
    user: Option<&str>,
    project: Option<&str>,
    prefix: Option<OsString>,
    launcher: Option<&Path>,
) -> InvariantStatus {
    let has = |text: Option<&str>, marker: &str| text.is_some_and(|t| t.contains(marker));
    if !has(user, USER_MARKER) && !has(project, PROJECT_MARKER) {
        if !has(user, ANY_LOOM_USER_HOOK) {
            return InvariantStatus::Skipped(
                "user-scope Loom hooks are not provisioned on this host".to_string(),
            );
        }
        return InvariantStatus::Violation(
            "~/.claude/settings.json carries Loom hooks but not the SessionStart \
             gh-front-env.sh entry, so interactive sessions and their subagents \
             spend GraphQL on plain `gh` reads; run `loom update` (re-runs \
             scripts/install/provision-hooks.sh)"
                .to_string(),
        );
    }
    let Some(prefix) = prefix else {
        return InvariantStatus::Violation(
            "the gh shim directory could not be created (`loom-daemon gh-shim path`)".to_string(),
        );
    };
    match session_env::first_gh(Some(&prefix)) {
        Some(gh) if session_env::classify_gh(&gh, launcher) != "bypassed" => InvariantStatus::Ok,
        other => InvariantStatus::Violation(format!(
            "the session PATH prefix {} resolves gh to {} — not a loom-daemon front or the \
             managed launcher",
            prefix.to_string_lossy(),
            other.map_or_else(|| "nothing".to_string(), |p| p.display().to_string())
        )),
    }
}
